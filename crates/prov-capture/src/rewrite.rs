use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const REWRITE_WINDOW_SECS: i64 = 14 * 24 * 60 * 60;
const SIDECAR_MTIME_MARGIN_SECS: i64 = 24 * 60 * 60;
const MAX_MTIME_CANDIDATES: usize = 200;
const MAX_SOURCES: usize = 50;
const MAX_HASH_ENTRIES: usize = 200_000;
const REWRITE_BUDGET: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum Label {
    Unknown,
    Mixed,
    Probable,
    Strong,
}

impl Label {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "u" => Some(Self::Unknown),
            "m" => Some(Self::Mixed),
            "p" => Some(Self::Probable),
            "s" => Some(Self::Strong),
            _ => None,
        }
    }

    fn as_char(self) -> char {
        match self {
            Self::Unknown => 'u',
            Self::Mixed => 'm',
            Self::Probable => 'p',
            Self::Strong => 's',
        }
    }
}

#[derive(Deserialize, Serialize)]
struct CommitReport {
    schema: String,
    repo_id: String,
    commit: String,
    parent: Option<String>,
    totals: Totals,
    coverage: String,
    #[serde(default)]
    coverage_unobserved_lines: usize,
    files: Vec<ReportFile>,
    patch_id: Option<String>,
    rewrite: RewriteInfo,
}

#[derive(Deserialize, Serialize)]
struct Totals {
    e: usize,
    ai_direct: usize,
    ai_strong: usize,
    ai_probable: usize,
    mixed: usize,
    unknown: usize,
    excluded: usize,
    unreadable_files: usize,
    ai_deterministic_pct: Option<f64>,
    ai_linked_pct: Option<f64>,
}

#[derive(Deserialize, Serialize)]
struct ReportFile {
    path: String,
    e: usize,
    ai_strong: usize,
    ai_probable: usize,
    mixed: usize,
    unknown: usize,
    excluded: usize,
    evidence: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    carried: Option<usize>,
}

#[derive(Deserialize, Serialize)]
struct RewriteInfo {
    method: String,
    sources: Vec<String>,
    carried_lines: usize,
}

#[derive(Deserialize, Serialize)]
struct LinesSidecar {
    schema: String,
    repo_id: String,
    commit: String,
    totals: LineTotals,
    files: Vec<LineFile>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    attributions: Vec<crate::commit::AttributionRange>,
}

#[derive(Deserialize, Serialize)]
struct LineTotals {
    e: usize,
    ai_strong: usize,
    ai_probable: usize,
    mixed: usize,
    unknown: usize,
}

#[derive(Deserialize, Serialize)]
struct LineFile {
    path: String,
    lines: Vec<LineEntry>,
}

#[derive(Clone, Deserialize, Serialize)]
struct LineEntry {
    line: usize,
    hash: String,
    label: String,
}

struct Source {
    sha: String,
    patch_id: Option<String>,
    sidecar: LinesSidecar,
}

struct Candidate {
    sha: String,
    time: i64,
    sidecar_path: PathBuf,
    patch_id: Option<String>,
}

struct CarryResult {
    method: &'static str,
    sources: Vec<String>,
    carried_lines: usize,
    carried_by_file: BTreeMap<String, usize>,
    carried_attributions: Vec<crate::commit::AttributionRange>,
}

struct CarryContext<'a> {
    root: &'a Path,
    commit: &'a str,
    started: Instant,
}

pub(crate) fn apply(root: &Path, store: &Path, commit: &str) -> Result<(), String> {
    let started = Instant::now();
    let commits_dir = store.join("commits");
    let report_path = commits_dir.join(format!("{commit}.json"));
    let sidecar_path = commits_dir.join(format!("{commit}.lines.json"));
    let mut report: CommitReport =
        serde_json::from_slice(&fs::read(&report_path).map_err(|error| error.to_string())?)
            .map_err(|error| error.to_string())?;
    let mut sidecar: LinesSidecar =
        serde_json::from_slice(&fs::read(&sidecar_path).map_err(|error| error.to_string())?)
            .map_err(|error| error.to_string())?;

    let carry = match collect_and_carry(root, &commits_dir, &mut report, &mut sidecar, started) {
        Ok(carry) => carry,
        Err(_) => return Ok(()),
    };
    if carry.method == "none" || carry.carried_lines == 0 {
        return Ok(());
    }

    for file in &mut report.files {
        file.carried = carry.carried_by_file.get(&file.path).copied();
    }
    replace_carried_attributions(&mut sidecar, carry.carried_attributions);
    recompute_totals(&mut report);
    sidecar.totals = LineTotals {
        e: report.totals.e,
        ai_strong: report.totals.ai_strong,
        ai_probable: report.totals.ai_probable,
        mixed: report.totals.mixed,
        unknown: report.totals.unknown,
    };
    report.rewrite = RewriteInfo {
        method: carry.method.to_owned(),
        sources: carry.sources,
        carried_lines: carry.carried_lines,
    };
    sort_sidecar(&mut sidecar);

    let sidecar_bytes = serde_json::to_vec(&sidecar).map_err(|error| error.to_string())?;
    let report_bytes = serde_json::to_vec(&report).map_err(|error| error.to_string())?;
    if started.elapsed() >= REWRITE_BUDGET {
        return Ok(());
    }
    crate::atomic_bytes(&sidecar_path, &sidecar_bytes, false).map_err(|error| error.to_string())?;
    crate::atomic_bytes(&report_path, &report_bytes, false).map_err(|error| error.to_string())?;
    Ok(())
}

fn collect_and_carry(
    root: &Path,
    commits_dir: &Path,
    report: &mut CommitReport,
    sidecar: &mut LinesSidecar,
    started: Instant,
) -> Result<CarryResult, String> {
    check_budget(started)?;
    if report.totals.e == 0 || sidecar_entry_count(sidecar) == 0 {
        return Ok(empty_result());
    }
    let mut hash_entries = sidecar_entry_count(sidecar);
    if hash_entries > MAX_HASH_ENTRIES {
        return Err("rewrite hash entry limit exceeded".into());
    }

    let target_time = commit_time(root, &report.commit)?;
    let target_patch_id = report.patch_id.clone();
    let ancestors = ancestor_set(root, &report.commit)?;
    let mtime_floor = target_time
        .saturating_sub(REWRITE_WINDOW_SECS)
        .saturating_sub(SIDECAR_MTIME_MARGIN_SECS);
    let mtime_ceiling = target_time.saturating_add(SIDECAR_MTIME_MARGIN_SECS);
    let mut mtime_candidates = Vec::new();
    for entry in fs::read_dir(commits_dir).map_err(|error| error.to_string())? {
        check_budget(started)?;
        let Ok(entry) = entry else {
            continue;
        };
        let sidecar_path = entry.path();
        let Some(sha) = sidecar_sha(&sidecar_path) else {
            continue;
        };
        if sha == report.commit || !valid_sha(&sha) {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        let Ok(modified) = metadata.modified() else {
            continue;
        };
        let Some(mtime) = system_time_seconds(modified) else {
            continue;
        };
        if !(mtime_floor..=mtime_ceiling).contains(&mtime) {
            continue;
        }
        mtime_candidates.push((modified, mtime, sha, sidecar_path));
    }
    mtime_candidates.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.2.cmp(&right.2)));
    mtime_candidates.truncate(MAX_MTIME_CANDIDATES);

    let candidate_shas: Vec<_> = mtime_candidates
        .iter()
        .map(|(_, _, sha, _)| sha.clone())
        .collect();
    let existing = existing_commits(root, &candidate_shas)?;
    let existing_shas: Vec<_> = candidate_shas
        .into_iter()
        .filter(|sha| existing.contains(sha))
        .collect();
    let commit_times = batch_commit_times(root, &existing_shas)?;

    let mut candidates = Vec::new();
    for (_, _, sha, sidecar_path) in mtime_candidates {
        check_budget(started)?;
        if !existing.contains(&sha) || ancestors.contains(&sha) {
            continue;
        }
        let Some(time) = commit_times.get(&sha).copied() else {
            continue;
        };
        if time > target_time || target_time.saturating_sub(time) > REWRITE_WINDOW_SECS {
            continue;
        }
        let report_value = fs::read(commits_dir.join(format!("{sha}.json")))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
        let Some(report_value) = report_value else {
            continue;
        };
        if report_value.get("skipped").is_some()
            || report_value["totals"]["e"].as_u64().unwrap_or_default() == 0
        {
            continue;
        }
        candidates.push(Candidate {
            sha,
            time,
            sidecar_path,
            patch_id: report_value
                .get("patch_id")
                .and_then(Value::as_str)
                .map(str::to_owned),
        });
    }
    candidates.sort_by(|left, right| {
        right
            .time
            .cmp(&left.time)
            .then_with(|| left.sha.cmp(&right.sha))
    });
    candidates.truncate(MAX_SOURCES);

    let mut sources = Vec::with_capacity(candidates.len());
    for candidate in candidates {
        check_budget(started)?;
        let Ok(bytes) = fs::read(&candidate.sidecar_path) else {
            continue;
        };
        let Ok(mut source_sidecar) = serde_json::from_slice::<LinesSidecar>(&bytes) else {
            continue;
        };
        if source_sidecar.totals.e == 0 {
            continue;
        }
        hash_entries = hash_entries.saturating_add(sidecar_entry_count(&source_sidecar));
        if hash_entries > MAX_HASH_ENTRIES {
            return Err("rewrite hash entry limit exceeded".into());
        }
        sort_sidecar(&mut source_sidecar);
        sources.push(Source {
            sha: candidate.sha,
            patch_id: candidate.patch_id,
            sidecar: source_sidecar,
        });
    }
    sources.sort_by(|left, right| left.sha.cmp(&right.sha));
    sort_sidecar(sidecar);
    let target_commit = report.commit.clone();
    let context = CarryContext {
        root,
        commit: &target_commit,
        started,
    };

    let patch_sources: Vec<_> = target_patch_id
        .as_deref()
        .filter(|patch_id| !patch_id.is_empty())
        .map(|patch_id| {
            sources
                .iter()
                .filter(|source| source.patch_id.as_deref() == Some(patch_id))
                .map(|source| source.sha.clone())
                .collect()
        })
        .unwrap_or_default();
    if !patch_sources.is_empty() {
        carry_matching_lines(
            sidecar,
            report,
            &sources,
            "patch_id",
            &patch_sources,
            &context,
        )
    } else {
        let sources = content_sources_for_target(sidecar, sources);
        carry_matching_lines(sidecar, report, &sources, "content", &[], &context)
    }
}

fn content_sources_for_target(target: &LinesSidecar, sources: Vec<Source>) -> Vec<Source> {
    let target_counts = line_multiset(target);
    sources
        .into_iter()
        .filter(|source| {
            let source_counts = line_multiset(&source.sidecar);
            let eligible = source_counts.values().sum::<usize>();
            if eligible == 0 {
                return false;
            }
            let overlap = source_counts
                .iter()
                .map(|(key, count)| count.min(target_counts.get(key).unwrap_or(&0)))
                .sum::<usize>();
            overlap.saturating_mul(2) >= eligible
        })
        .collect()
}

fn line_multiset(sidecar: &LinesSidecar) -> BTreeMap<(String, String), usize> {
    let mut counts = BTreeMap::new();
    for file in &sidecar.files {
        for line in &file.lines {
            *counts
                .entry((file.path.clone(), line.hash.clone()))
                .or_insert(0) += 1;
        }
    }
    counts
}

fn carry_matching_lines(
    target: &mut LinesSidecar,
    report: &mut CommitReport,
    sources: &[Source],
    method: &'static str,
    patch_sources: &[String],
    context: &CarryContext<'_>,
) -> Result<CarryResult, String> {
    if sources.is_empty() {
        return Ok(empty_result());
    }
    let mut used = HashSet::new();
    let mut used_sources = BTreeSet::new();
    let mut carried_by_file = BTreeMap::new();
    let mut carried_attributions = Vec::new();
    let mut carried_lines = 0;
    let patch_source_set: BTreeSet<_> = patch_sources.iter().cloned().collect();

    for target_file in &mut target.files {
        check_budget(context.started)?;
        let lengths = if method == "content" {
            load_commit_file(context.root, context.commit, &target_file.path)
                .ok()
                .map(|bytes| normalized_line_lengths(&bytes))
        } else {
            None
        };
        let mut file_carried = 0;
        for target_line in &mut target_file.lines {
            check_budget(context.started)?;
            if method == "content"
                && lengths
                    .as_ref()
                    .and_then(|values| values.get(target_line.line.saturating_sub(1)))
                    .copied()
                    .unwrap_or(0)
                    < 8
            {
                continue;
            }
            let mut matching = Vec::new();
            for source in sources {
                if method == "patch_id" && !patch_source_set.contains(&source.sha) {
                    continue;
                }
                let Some(source_file) = source
                    .sidecar
                    .files
                    .iter()
                    .find(|file| file.path == target_file.path)
                else {
                    continue;
                };
                let Some((index, line)) =
                    source_file.lines.iter().enumerate().find(|(index, line)| {
                        line.hash == target_line.hash
                            && !used.contains(&(
                                source.sha.clone(),
                                target_file.path.clone(),
                                *index,
                            ))
                    })
                else {
                    continue;
                };
                let Some(label) = Label::parse(&line.label) else {
                    continue;
                };
                used.insert((source.sha.clone(), target_file.path.clone(), index));
                matching.push((source, label, line));
            }
            let Some(weakest) = matching.iter().map(|(_, label, _)| *label).min() else {
                continue;
            };
            used_sources.extend(matching.iter().map(|(source, _, _)| source.sha.clone()));
            let selected_source = matching
                .iter()
                .filter(|(_, label, _)| *label == weakest)
                .min_by_key(|(source, _, _)| source.sha.as_str());
            let current = Label::parse(&target_line.label).unwrap_or(Label::Unknown);
            let next = if method == "patch_id" {
                (current <= weakest).then_some(weakest)
            } else if weakest != Label::Unknown && current < weakest {
                Some(weakest.min(Label::Probable))
            } else {
                None
            };
            let Some(next) = next else {
                continue;
            };
            if current != next {
                adjust_file_counts(report, &target_file.path, current, next);
            }
            if let Some((source, _, source_line)) = selected_source {
                let source_range = source
                    .sidecar
                    .attributions
                    .iter()
                    .find(|range| {
                        range.path == target_file.path
                            && range.start_line <= source_line.line
                            && source_line.line <= range.end_line
                    })
                    .cloned();
                carried_attributions.push(carried_attribution(
                    &target_file.path,
                    target_line.line,
                    next,
                    method,
                    source_range,
                ));
            }
            target_line.label = next.as_char().to_string();
            file_carried += 1;
            carried_lines += 1;
        }
        if file_carried > 0 {
            carried_by_file.insert(target_file.path.clone(), file_carried);
        }
    }
    let sources = used_sources.into_iter().collect();
    Ok(CarryResult {
        method,
        sources,
        carried_lines,
        carried_by_file,
        carried_attributions,
    })
}

fn adjust_file_counts(report: &mut CommitReport, path: &str, previous: Label, next: Label) {
    let Some(file) = report.files.iter_mut().find(|file| file.path == path) else {
        return;
    };
    let previous_count = match previous {
        Label::Unknown => &mut file.unknown,
        Label::Mixed => &mut file.mixed,
        Label::Probable => &mut file.ai_probable,
        Label::Strong => &mut file.ai_strong,
    };
    *previous_count = previous_count.saturating_sub(1);
    match next {
        Label::Unknown => file.unknown += 1,
        Label::Mixed => file.mixed += 1,
        Label::Probable => file.ai_probable += 1,
        Label::Strong => file.ai_strong += 1,
    }
}

fn carried_attribution(
    path: &str,
    line: usize,
    label: Label,
    method: &str,
    source: Option<crate::commit::AttributionRange>,
) -> crate::commit::AttributionRange {
    let category = match label {
        Label::Strong => "ai_strong",
        Label::Probable => "ai_probable",
        Label::Mixed => "mixed",
        Label::Unknown => "unknown",
    };
    let primary_basis = if method == "patch_id" {
        "REBASE_PATCH_ID"
    } else {
        "MOVE_LINEAGE"
    };
    let mut supporting_basis = source
        .as_ref()
        .map(|range| range.supporting_basis.clone())
        .unwrap_or_default();
    if let Some(basis) = source
        .as_ref()
        .and_then(|range| range.primary_basis.as_ref())
    {
        if !supporting_basis.contains(basis) {
            supporting_basis.push(basis.clone());
        }
    }
    let mut limiting_basis = source
        .as_ref()
        .map(|range| range.limiting_basis.clone())
        .unwrap_or_default();
    if source.is_none()
        && !limiting_basis
            .iter()
            .any(|basis| basis == "INCOMPLETE_SOURCE")
    {
        limiting_basis.push("INCOMPLETE_SOURCE".into());
    }
    if label == Label::Unknown {
        supporting_basis.clear();
        limiting_basis.clear();
    }
    let fallback_score = match (method, label) {
        ("patch_id", Label::Strong) => Some(0.82),
        ("patch_id", Label::Probable) => Some(0.72),
        ("content", Label::Probable) => Some(0.60),
        (_, Label::Mixed) => None,
        (_, Label::Unknown) => None,
        _ => Some(0.60),
    };
    let score = match (method, label) {
        (_, Label::Unknown) => None,
        ("content", Label::Probable) => Some(0.60),
        (_, _) => source
            .as_ref()
            .and_then(|range| range.score)
            .or(fallback_score),
    };
    let confidence_band = match label {
        Label::Unknown => "none",
        Label::Mixed | Label::Strong | Label::Probable => {
            score.map_or("none", crate::commit::confidence_band)
        }
    };
    crate::commit::AttributionRange {
        path: path.into(),
        start_line: line,
        end_line: line,
        end_line_exclusive: Some(line + 1),
        category: category.into(),
        score,
        confidence_band: confidence_band.into(),
        primary_basis: (label != Label::Unknown).then(|| primary_basis.into()),
        supporting_basis,
        limiting_basis,
        source: source.as_ref().and_then(|range| range.source.clone()),
        operation: source.as_ref().and_then(|range| range.operation.clone()),
        algorithm_profile: "deterministic-v1".into(),
        score_profile: "score-v1".into(),
        exclusion_classifier: source
            .as_ref()
            .and_then(|range| range.exclusion_classifier.clone()),
        exclusion_reason: source
            .as_ref()
            .and_then(|range| range.exclusion_reason.clone()),
    }
}

fn replace_carried_attributions(
    sidecar: &mut LinesSidecar,
    carried: Vec<crate::commit::AttributionRange>,
) {
    let mut lines = BTreeMap::new();
    for range in sidecar.attributions.drain(..) {
        for line in range.start_line..=range.end_line {
            let mut singleton = range.clone();
            singleton.start_line = line;
            singleton.end_line = line;
            lines.insert((singleton.path.clone(), line), singleton);
        }
    }
    for range in carried {
        lines.insert((range.path.clone(), range.start_line), range);
    }
    for (_, range) in lines {
        if let Some(previous) = sidecar.attributions.last_mut() {
            if same_attribution(previous, &range) && previous.end_line + 1 == range.start_line {
                previous.end_line = range.end_line;
                previous.end_line_exclusive = Some(range.end_line + 1);
                continue;
            }
        }
        sidecar.attributions.push(range);
    }
}

fn same_attribution(
    left: &crate::commit::AttributionRange,
    right: &crate::commit::AttributionRange,
) -> bool {
    left.path == right.path
        && left.category == right.category
        && left.score == right.score
        && left.confidence_band == right.confidence_band
        && left.primary_basis == right.primary_basis
        && left.supporting_basis == right.supporting_basis
        && left.limiting_basis == right.limiting_basis
        && left.source == right.source
        && left.operation == right.operation
        && left.algorithm_profile == right.algorithm_profile
        && left.score_profile == right.score_profile
        && left.exclusion_classifier == right.exclusion_classifier
        && left.exclusion_reason == right.exclusion_reason
}

fn recompute_totals(report: &mut CommitReport) {
    let mut totals = Totals {
        e: 0,
        ai_direct: report.totals.ai_direct,
        ai_strong: 0,
        ai_probable: 0,
        mixed: 0,
        unknown: 0,
        excluded: 0,
        unreadable_files: report.totals.unreadable_files,
        ai_deterministic_pct: None,
        ai_linked_pct: None,
    };
    let mut covered = 0;
    for file in &report.files {
        totals.e += file.e;
        totals.ai_strong += file.ai_strong;
        totals.ai_probable += file.ai_probable;
        totals.mixed += file.mixed;
        totals.unknown += file.unknown;
        totals.excluded += file.excluded;
        if (file.evidence != "none" && file.evidence != "missing_blob")
            || file.carried.unwrap_or_default() > 0
        {
            covered += file.e;
        }
    }
    totals.ai_deterministic_pct = percentage(totals.ai_direct + totals.ai_strong, totals.e);
    totals.ai_linked_pct = percentage(
        totals.ai_direct + totals.ai_strong + totals.ai_probable + totals.mixed,
        totals.e,
    );
    report.coverage = crate::commit::coverage_band_with_unobserved(
        covered,
        totals.e,
        report.coverage_unobserved_lines,
    )
    .to_owned();
    report.totals = totals;
}

fn percentage(numerator: usize, denominator: usize) -> Option<f64> {
    if denominator == 0 {
        None
    } else {
        let tenths =
            ((numerator as u128 * 1000 + denominator as u128 / 2) / denominator as u128) as u64;
        Some(tenths as f64 / 10.0)
    }
}

fn empty_result() -> CarryResult {
    CarryResult {
        method: "none",
        sources: Vec::new(),
        carried_lines: 0,
        carried_by_file: BTreeMap::new(),
        carried_attributions: Vec::new(),
    }
}

fn sidecar_entry_count(sidecar: &LinesSidecar) -> usize {
    sidecar.files.iter().map(|file| file.lines.len()).sum()
}

fn sort_sidecar(sidecar: &mut LinesSidecar) {
    sidecar
        .files
        .sort_by(|left, right| left.path.cmp(&right.path));
    for file in &mut sidecar.files {
        file.lines.sort_by_key(|line| line.line);
    }
    sidecar.attributions.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then_with(|| left.start_line.cmp(&right.start_line))
    });
}

fn sidecar_sha(path: &Path) -> Option<String> {
    path.file_name()?
        .to_str()?
        .strip_suffix(".lines.json")
        .map(str::to_owned)
}

fn valid_sha(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn ancestor_set(root: &Path, commit: &str) -> Result<HashSet<String>, String> {
    let output = git_output(root, &["rev-list", "-n", "10000", commit])?;
    if !output.status.success() {
        return Err("could not read commit ancestry".into());
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::to_owned)
        .collect())
}

fn commit_time(root: &Path, commit: &str) -> Result<i64, String> {
    let output = git_output(root, &["show", "-s", "--format=%ct", commit])?;
    if !output.status.success() {
        return Err("could not read commit time".into());
    }
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .map_err(|error: std::num::ParseIntError| error.to_string())
}

fn existing_commits(root: &Path, commits: &[String]) -> Result<BTreeSet<String>, String> {
    if commits.is_empty() {
        return Ok(BTreeSet::new());
    }
    let input = format!("{}\n", commits.join("\n"));
    let output = git_input(
        root,
        &["cat-file", "--batch-check=%(objectname) %(objecttype)"],
        input.as_bytes(),
    )?;
    if !output.status.success() {
        return Err("could not batch-check rewrite commits".into());
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let sha = fields.next()?;
            (fields.next()? == "commit").then(|| sha.to_owned())
        })
        .collect())
}

fn batch_commit_times(root: &Path, commits: &[String]) -> Result<BTreeMap<String, i64>, String> {
    if commits.is_empty() {
        return Ok(BTreeMap::new());
    }
    let input = format!("{}\n", commits.join("\n"));
    let output = git_input(
        root,
        &["log", "--no-walk=unsorted", "--format=%H %ct", "--stdin"],
        input.as_bytes(),
    )?;
    if !output.status.success() {
        return Err("could not batch-read rewrite commit times".into());
    }
    let mut times = BTreeMap::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let mut fields = line.split_whitespace();
        let (Some(sha), Some(time)) = (fields.next(), fields.next()) else {
            continue;
        };
        if let Ok(time) = time.parse() {
            times.insert(sha.to_owned(), time);
        }
    }
    Ok(times)
}

fn git_input(root: &Path, args: &[&str], input: &[u8]) -> Result<Output, String> {
    let mut command = Command::new("git");
    command
        .args(args)
        .current_dir(root)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    crate::configure_process_command(&mut command, false);
    let mut child = command.spawn().map_err(|error| error.to_string())?;
    let write_result = child
        .stdin
        .take()
        .ok_or("could not open git stdin")?
        .write_all(input);
    if let Err(error) = write_result {
        let _ = child.wait();
        return Err(error.to_string());
    }
    child.wait_with_output().map_err(|error| error.to_string())
}

fn git_output(root: &Path, args: &[&str]) -> Result<Output, String> {
    let mut command = Command::new("git");
    command
        .args(args)
        .current_dir(root)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null());
    crate::configure_process_command(&mut command, false);
    command.output().map_err(|error| error.to_string())
}

fn load_commit_file(root: &Path, commit: &str, path: &str) -> Result<Vec<u8>, String> {
    let object = format!("{commit}:{path}");
    let output = git_output(root, &["cat-file", "blob", &object])?;
    if !output.status.success() {
        return Err("could not read committed file".into());
    }
    Ok(output.stdout)
}

fn normalized_line_lengths(bytes: &[u8]) -> Vec<usize> {
    let mut lines: Vec<_> = bytes.split(|byte| *byte == b'\n').collect();
    if bytes.last() == Some(&b'\n') {
        lines.pop();
    }
    lines
        .into_iter()
        .map(|line| String::from_utf8_lossy(line).trim().chars().count())
        .collect()
}

fn check_budget(started: Instant) -> Result<(), String> {
    if started.elapsed() >= REWRITE_BUDGET {
        Err("rewrite budget exceeded".into())
    } else {
        Ok(())
    }
}

fn system_time_seconds(time: SystemTime) -> Option<i64> {
    i64::try_from(time.duration_since(UNIX_EPOCH).ok()?.as_secs()).ok()
}

#[cfg(test)]
mod tests {
    use super::LinesSidecar;

    #[test]
    fn old_format_sidecar_loads_without_attribution_ranges() {
        let bytes = br#"{"schema":"prov.lines/0.1","repo_id":"repo","commit":"commit","totals":{"e":1,"ai_strong":0,"ai_probable":0,"mixed":0,"unknown":1},"files":[{"path":"file.txt","lines":[{"line":2,"hash":"0123456789abcdef","label":"u"}]}]}"#;
        let sidecar: LinesSidecar = serde_json::from_slice(bytes).unwrap();
        assert!(sidecar.attributions.is_empty());
        let round_trip = serde_json::to_value(sidecar).unwrap();
        assert!(round_trip.get("attributions").is_none());
    }

    #[test]
    fn missing_optional_range_fields_default_safely() {
        let bytes = br#"{"path":"file.txt","startLine":2,"endLine":2}"#;
        let range: crate::commit::AttributionRange = serde_json::from_slice(bytes).unwrap();
        assert_eq!(range.category, "unknown");
        assert!(range.score.is_none());
        assert_eq!(range.confidence_band, "none");
        assert!(range.primary_basis.is_none());
        assert!(range.supporting_basis.is_empty());
        assert!(range.limiting_basis.is_empty());
        assert!(range.source.is_none());
        assert!(range.operation.is_none());
        assert_eq!(range.algorithm_profile, "deterministic-v1");
        assert_eq!(range.score_profile, "score-v1");
    }
}
