use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

const REWRITE_WINDOW_SECS: i64 = 14 * 24 * 60 * 60;
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
}

struct CarryResult {
    method: &'static str,
    sources: Vec<String>,
    carried_lines: usize,
    carried_by_file: BTreeMap<String, usize>,
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
    if carry.method == "none" {
        return Ok(());
    }

    for file in &mut report.files {
        file.carried = carry.carried_by_file.get(&file.path).copied();
    }
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
    let lower_time = target_time.saturating_sub(REWRITE_WINDOW_SECS);
    let mut sidecar_paths: Vec<PathBuf> = fs::read_dir(commits_dir)
        .map_err(|error| error.to_string())?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(".lines.json"))
        })
        .collect();
    sidecar_paths.sort();

    let mut candidates = Vec::new();
    for sidecar_path in sidecar_paths {
        check_budget(started)?;
        let Some(sha) = sidecar_sha(&sidecar_path) else {
            continue;
        };
        if sha == report.commit || ancestors.contains(&sha) || !valid_sha(&sha) {
            continue;
        }
        if !git_succeeds(root, &["cat-file", "-e", &format!("{sha}^{{commit}}")]).unwrap_or(false) {
            continue;
        }
        let Ok(time) = commit_time(root, &sha) else {
            continue;
        };
        if time > target_time || time < lower_time {
            continue;
        }
        let sidecar_e = fs::read(&sidecar_path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<LinesSidecar>(&bytes).ok())
            .map(|sidecar| sidecar.totals.e);
        if !sidecar_e.is_some_and(|e| e > 0) {
            continue;
        }
        candidates.push(Candidate {
            sha,
            time,
            sidecar_path,
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
        let mut source_sidecar: LinesSidecar = serde_json::from_slice(
            &fs::read(&candidate.sidecar_path).map_err(|error| error.to_string())?,
        )
        .map_err(|error| error.to_string())?;
        hash_entries = hash_entries.saturating_add(sidecar_entry_count(&source_sidecar));
        if hash_entries > MAX_HASH_ENTRIES {
            return Err("rewrite hash entry limit exceeded".into());
        }
        sort_sidecar(&mut source_sidecar);
        let commit_json = fs::read(commits_dir.join(format!("{}.json", candidate.sha)))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
        let Some(commit_json) = commit_json else {
            continue;
        };
        let patch_id = commit_json
            .get("patch_id")
            .and_then(Value::as_str)
            .map(str::to_owned);
        sources.push(Source {
            sha: candidate.sha,
            patch_id,
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
        carry_matching_lines(sidecar, report, &sources, "content", &[], &context)
    }
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
                matching.push((source.sha.as_str(), label));
            }
            let Some(weakest) = matching.iter().map(|(_, label)| *label).min() else {
                continue;
            };
            used_sources.extend(matching.iter().map(|(sha, _)| (*sha).to_owned()));
            let current = Label::parse(&target_line.label).unwrap_or(Label::Unknown);
            let next = if method == "patch_id" {
                Some(weakest)
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
        if file.evidence != "none" && file.evidence != "missing_blob" {
            covered += file.e;
        }
    }
    totals.ai_deterministic_pct = percentage(totals.ai_direct + totals.ai_strong, totals.e);
    totals.ai_linked_pct = percentage(
        totals.ai_direct + totals.ai_strong + totals.ai_probable + totals.mixed,
        totals.e,
    );
    report.coverage = coverage(covered, totals.e).to_owned();
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

fn coverage(covered: usize, total: usize) -> &'static str {
    if total == 0 || covered == 0 {
        "none"
    } else if covered.saturating_mul(3) < total {
        "low"
    } else if covered.saturating_mul(3) < total.saturating_mul(2) {
        "medium"
    } else {
        "high"
    }
}

fn empty_result() -> CarryResult {
    CarryResult {
        method: "none",
        sources: Vec::new(),
        carried_lines: 0,
        carried_by_file: BTreeMap::new(),
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

fn git_succeeds(root: &Path, args: &[&str]) -> Result<bool, String> {
    git_output(root, args).map(|output| output.status.success())
}

fn git_output(root: &Path, args: &[&str]) -> Result<Output, String> {
    Command::new("git")
        .args(args)
        .current_dir(root)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .output()
        .map_err(|error| error.to_string())
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
