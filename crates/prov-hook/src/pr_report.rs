use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_COMMITS: usize = 500;
const ENGINE_VERSION: &str = env!("CARGO_PKG_VERSION");
const ALGORITHM_VERSION: &str = "derive-v1";
const LINE_SURVIVAL_VERSION: &str = prov_capture::commit::LINE_SURVIVAL_VERSION;
const FORMULA_VERSION: &str = "strict-v1";
const ELIGIBILITY_VERSION: &str = "non-blank-added-v1";
const COVERAGE_VERSION: &str = "coverage-v3";

#[derive(Default)]
struct Options {
    repo: Option<PathBuf>,
    base: Option<String>,
    head: Option<String>,
    number: Option<u64>,
    title: Option<String>,
    author: Option<String>,
    status: Option<String>,
    url: Option<String>,
    prs_file: Option<PathBuf>,
    output: Option<PathBuf>,
    no_derive: bool,
}

#[derive(Clone, Deserialize)]
struct PrInput {
    number: Option<u64>,
    title: Option<String>,
    author: Option<String>,
    status: Option<String>,
    url: Option<String>,
    base: String,
    head: String,
}

#[derive(Default)]
struct FileTotals {
    eligible: usize,
    linked: usize,
}

pub fn command(args: impl Iterator<Item = String>) -> Result<(Vec<u8>, Option<PathBuf>), String> {
    let options = parse_options(args)?;
    let repo = options
        .repo
        .clone()
        .or_else(|| std::env::current_dir().ok())
        .ok_or("could not determine current directory")?;
    let repo = resolve_repo(&repo)?;
    let repo_id = prov_capture::commit::reports_for_commits(&repo, &[])?.repo_id;

    let mut prs = if let Some(path) = &options.prs_file {
        let bytes = fs::read(path)
            .map_err(|error| format!("could not read {}: {error}", path.display()))?;
        let inputs: Vec<PrInput> =
            serde_json::from_slice(&bytes).map_err(|error| format!("invalid PR JSON: {error}"))?;
        let mut reports = Vec::with_capacity(inputs.len());
        for input in inputs {
            validate_status(input.status.as_deref())?;
            reports.push(build_pr_report(&repo, &input, options.no_derive)?);
        }
        reports
    } else {
        let input = PrInput {
            number: options.number,
            title: options.title,
            author: options.author,
            status: options.status,
            url: options.url,
            base: options
                .base
                .ok_or("--base is required unless --prs-file is used")?,
            head: options
                .head
                .ok_or("--head is required unless --prs-file is used")?,
        };
        validate_status(input.status.as_deref())?;
        vec![build_pr_report(&repo, &input, options.no_derive)?]
    };

    let generated_at = iso8601_now();
    for pr in &mut prs {
        pr.as_object_mut()
            .ok_or("PR report is not a JSON object")?
            .insert("created_at".into(), Value::String(generated_at.clone()));
    }

    let output = if options.prs_file.is_some() {
        json!({
            "schema": "prov.dashboard/0.1",
            "generated_at": generated_at,
            "repo_id": repo_id,
            "prs": prs,
        })
    } else {
        prs.into_iter().next().ok_or("PR report was not created")?
    };
    let bytes = serde_json::to_vec(&output).map_err(|error| error.to_string())?;
    Ok((bytes, options.output))
}

fn parse_options(args: impl Iterator<Item = String>) -> Result<Options, String> {
    let mut options = Options::default();
    let mut args = args;
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--repo" => {
                options.repo = Some(PathBuf::from(args.next().ok_or("--repo needs a path")?))
            }
            "--base" => options.base = Some(args.next().ok_or("--base needs a revision")?),
            "--head" => options.head = Some(args.next().ok_or("--head needs a revision")?),
            "--pr" => {
                options.number = Some(
                    args.next()
                        .ok_or("--pr needs a number")?
                        .parse()
                        .map_err(|_| "--pr must be an integer")?,
                )
            }
            "--title" => options.title = Some(args.next().ok_or("--title needs text")?),
            "--author" => options.author = Some(args.next().ok_or("--author needs text")?),
            "--status" => options.status = Some(args.next().ok_or("--status needs a value")?),
            "--url" => options.url = Some(args.next().ok_or("--url needs text")?),
            "--prs-file" => {
                options.prs_file =
                    Some(PathBuf::from(args.next().ok_or("--prs-file needs a path")?))
            }
            "--out" => {
                options.output = Some(PathBuf::from(args.next().ok_or("--out needs a path")?))
            }
            "--no-derive" => options.no_derive = true,
            _ => return Err(format!("unknown pr-report argument: {argument}")),
        }
    }
    if options.prs_file.is_some()
        && (options.base.is_some()
            || options.head.is_some()
            || options.number.is_some()
            || options.title.is_some()
            || options.author.is_some()
            || options.status.is_some()
            || options.url.is_some())
    {
        return Err("--prs-file cannot be combined with single-PR metadata".into());
    }
    if options.prs_file.is_none() && (options.base.is_none() || options.head.is_none()) {
        return Err("--base and --head are required unless --prs-file is used".into());
    }
    Ok(options)
}

fn validate_status(status: Option<&str>) -> Result<(), String> {
    if status.is_some_and(|value| !matches!(value, "open" | "merged" | "closed")) {
        return Err("--status must be open, merged, or closed".into());
    }
    Ok(())
}

fn resolve_repo(repo: &Path) -> Result<PathBuf, String> {
    let output = git(repo, &["rev-parse", "--show-toplevel"])?;
    if !output.status.success() {
        return Err("not a readable Git repository".into());
    }
    let root = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
    Ok(root.canonicalize().unwrap_or(root))
}

fn build_pr_report(repo: &Path, input: &PrInput, no_derive: bool) -> Result<Value, String> {
    if input.base.trim().is_empty() || input.head.trim().is_empty() {
        return Err("PR base and head must not be empty".into());
    }
    let range = resolve_commit_range(repo, &input.base, &input.head)?;
    let base = range.base;
    let head = range.head;
    let merge_base = range.merge_base;
    let commits = range.commits;
    let truncated = range.truncated;
    let files_changed = merge_base
        .as_deref()
        .map(|merge_base| changed_files_count(repo, merge_base, &head))
        .transpose()?;

    let mut reports = prov_capture::commit::reports_for_commits(repo, &commits)?.reports;
    if !no_derive {
        let missing: Vec<_> = commits
            .iter()
            .zip(&reports)
            .filter_map(|(commit, report)| report.is_none().then_some(commit.clone()))
            .collect();
        if !missing.is_empty() {
            let _ = prov_capture::commit::enqueue_git_commits(repo, &missing);
            let _ = prov_capture::commit::derive_pending(repo);
            reports = prov_capture::commit::reports_for_commits(repo, &commits)?.reports;
        }
    }
    let line_sidecars = prov_capture::commit::lines_for_commits(repo, &commits)?;

    let mut derived = 0usize;
    let mut files = BTreeMap::<String, FileTotals>::new();
    let mut eligible = 0usize;
    let mut ai_direct = 0usize;
    let mut ai_strong = 0usize;
    let mut ai_probable = 0usize;
    let mut mixed = 0usize;
    let mut unknown = 0usize;
    let mut excluded = 0usize;
    let mut evidence = BTreeMap::from([
        ("exact", 0usize),
        ("time_cut", 0usize),
        ("reconstructed", 0usize),
        ("none", 0usize),
    ]);
    let mut covered = 0usize;
    let mut unobserved_lines = 0usize;
    let mut carried_lines = 0usize;
    let mut metrics_available = true;
    let mut tool_commits = BTreeSet::new();
    let mut single_commit_coverage = None;

    for ((_commit, report_bytes), sidecar) in commits.iter().zip(reports).zip(line_sidecars) {
        let Some(report_bytes) = report_bytes else {
            continue;
        };
        let report: Value = match serde_json::from_slice::<Value>(&report_bytes) {
            Ok(value) if value["schema"] == "prov.commit/0.1" => value,
            _ => continue,
        };
        if commits.len() == 1 {
            single_commit_coverage = report["coverage"].as_str().map(str::to_owned);
        }
        let Some(totals) = report.get("totals") else {
            continue;
        };
        let Some(e) = totals["e"].as_u64().map(|value| value as usize) else {
            metrics_available = false;
            continue;
        };
        let (
            Some(direct),
            Some(strong),
            Some(probable),
            Some(mixed_lines),
            Some(unknown_lines),
            Some(excluded_lines),
        ) = (
            totals["ai_direct"].as_u64(),
            totals["ai_strong"].as_u64(),
            totals["ai_probable"].as_u64(),
            totals["mixed"].as_u64(),
            totals["unknown"].as_u64(),
            totals["excluded"].as_u64(),
        )
        else {
            metrics_available = false;
            continue;
        };
        derived += 1;
        unobserved_lines += report["coverage_unobserved_lines"]
            .as_u64()
            .map(|value| value as usize)
            .or_else(|| {
                sidecar
                    .as_deref()
                    .map(prov_capture::commit::unobserved_eligible_lines_from_sidecar)
            })
            .unwrap_or_default();
        if let Some(commit) = report["commit"].as_str() {
            tool_commits.insert(commit.to_owned());
        }
        if let Some(sources) = report["rewrite"]["sources"].as_array() {
            tool_commits.extend(sources.iter().filter_map(Value::as_str).map(str::to_owned));
        }
        eligible += e;
        ai_direct += direct as usize;
        ai_strong += strong as usize;
        ai_probable += probable as usize;
        mixed += mixed_lines as usize;
        unknown += unknown_lines as usize;
        excluded += excluded_lines as usize;

        let Some(report_files) = report["files"].as_array() else {
            metrics_available = false;
            continue;
        };
        for file in report_files {
            let (Some(path), Some(file_e)) = (
                file["path"].as_str(),
                file["e"].as_u64().map(|value| value as usize),
            ) else {
                metrics_available = false;
                continue;
            };
            let linked = ["ai_direct", "ai_strong", "ai_probable", "mixed"]
                .iter()
                .filter_map(|key| file.get(key).and_then(Value::as_u64))
                .map(|value| value as usize)
                .sum::<usize>();
            let file_totals = files.entry(path.to_owned()).or_default();
            file_totals.eligible += file_e;
            file_totals.linked += linked;

            let kind = file["evidence"].as_str().unwrap_or("none");
            let bucket = match kind {
                "exact" => "exact",
                "time_cut" => "time_cut",
                "reconstructed" => "reconstructed",
                _ => "none",
            };
            *evidence.get_mut(bucket).expect("fixed evidence bucket") += file_e;
            if !matches!(kind, "none" | "missing_blob") {
                covered += file_e;
            }
        }
        if let Some(value) = report["rewrite"]["carried_lines"].as_u64() {
            carried_lines += value as usize;
        } else {
            metrics_available = false;
        }
    }

    let underived = commits.len().saturating_sub(derived);
    let complete = underived == 0 && !truncated && metrics_available;
    let pr = json!({
        "number": input.number,
        "title": input.title,
        "author": input.author,
        "status": input.status,
        "url": input.url,
        "base": base,
        "merge_base": merge_base,
        "head": head,
    });
    let lines = if complete {
        json!({
            "eligible": eligible,
            "ai_direct": ai_direct,
            "ai_strong": ai_strong,
            "ai_probable": ai_probable,
            "mixed": mixed,
            "unknown": unknown,
            "excluded": excluded,
        })
    } else {
        json!({
            "eligible": null,
            "ai_direct": null,
            "ai_strong": null,
            "ai_probable": null,
            "mixed": null,
            "unknown": null,
            "excluded": null,
        })
    };
    let evidence_json = if complete {
        json!(evidence)
    } else {
        json!({"exact":null,"time_cut":null,"reconstructed":null,"none":null})
    };
    let top_files = if complete {
        let mut entries: Vec<_> = files.into_iter().collect();
        entries.sort_by(|(left_path, left), (right_path, right)| {
            right
                .eligible
                .cmp(&left.eligible)
                .then_with(|| left_path.cmp(right_path))
        });
        json!(entries
            .into_iter()
            .take(20)
            .map(|(path, totals)| json!({
                "path": path,
                "eligible": totals.eligible,
                "ai_linked": totals.linked,
            }))
            .collect::<Vec<_>>())
    } else {
        Value::Null
    };
    let deterministic_pct = if complete {
        prov_capture::commit::percentage_tenths(ai_direct + ai_strong, eligible)
    } else {
        None
    };
    let linked_pct = if complete {
        prov_capture::commit::percentage_tenths(
            ai_direct + ai_strong + ai_probable + mixed,
            eligible,
        )
    } else {
        None
    };
    let coverage_band = coverage_band_for_range(
        complete,
        commits.len(),
        single_commit_coverage.as_deref(),
        covered,
        eligible,
        unobserved_lines,
    );
    let tool_commits: Vec<_> = tool_commits.into_iter().collect();
    let observed_tools = prov_capture::commit::tools_for_commits(repo, &tool_commits)
        .ok()
        .map(|per_commit| {
            per_commit
                .into_iter()
                .flatten()
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect::<Vec<_>>()
        });
    let tools = if complete {
        observed_tools.clone()
    } else {
        None
    };
    let coverage_dimensions = coverage_dimensions(
        derived,
        commits.len(),
        eligible,
        &evidence,
        complete,
        observed_tools.as_deref(),
    );
    let file_ranges = crate::summary::collect_all_file_ranges(repo, &commits)?;

    Ok(json!({
        "schema": "prov.pr-report/0.1",
        "versions": versions(),
        "pr": pr,
        "commits": {
            "total": commits.len(),
            "derived": derived,
            "underived": underived,
            "truncated": truncated,
        },
        "files_changed": files_changed,
        "lines": lines,
        "ai_deterministic_pct": deterministic_pct,
        "ai_linked_pct": linked_pct,
        "evidence": evidence_json,
        "coverage": {
            "band": coverage_band,
            "analysis": coverage_dimensions.analysis,
            "content": coverage_dimensions.content,
            "source": coverage_dimensions.source,
            "history": coverage_dimensions.history,
        },
        "tools": tools,
        "carried_lines": complete.then_some(carried_lines),
        "top_files": top_files,
        "file_ranges": file_ranges,
    }))
}

fn coverage_band_for_range(
    complete: bool,
    commit_count: usize,
    single_commit_coverage: Option<&str>,
    covered: usize,
    eligible: usize,
    unobserved_lines: usize,
) -> Value {
    if !complete {
        return Value::Null;
    }
    if commit_count == 1 {
        return json!(single_commit_coverage.unwrap_or_else(|| {
            prov_capture::commit::coverage_band_with_unobserved(covered, eligible, unobserved_lines)
        }));
    }
    json!(prov_capture::commit::coverage_band_with_unobserved(
        covered,
        eligible,
        unobserved_lines,
    ))
}

pub(crate) struct CommitRange {
    pub base: String,
    pub head: String,
    pub merge_base: Option<String>,
    pub commits: Vec<String>,
    pub truncated: bool,
}

pub(crate) fn resolve_commit_range(
    repo: &Path,
    requested_base: &str,
    requested_head: &str,
) -> Result<CommitRange, String> {
    if requested_base.trim().is_empty() || requested_head.trim().is_empty() {
        return Err("PR base and head must not be empty".into());
    }
    let base = resolve_revision(repo, requested_base)?;
    let head = resolve_revision(repo, requested_head)?;
    let merge_base = resolve_merge_base(repo, &base, &head)?;
    let range_base = merge_base.as_deref().unwrap_or(&base);
    let range = format!("{range_base}..{head}");
    let commits_output = git(
        repo,
        &["rev-list", "--no-merges", "--max-count=501", &range],
    )?;
    if !commits_output.status.success() {
        return Err("could not read PR commits from Git".into());
    }
    let mut commits: Vec<String> = String::from_utf8_lossy(&commits_output.stdout)
        .lines()
        .map(str::to_owned)
        .collect();
    let truncated = commits.len() > MAX_COMMITS;
    commits.truncate(MAX_COMMITS);
    Ok(CommitRange {
        base,
        head,
        merge_base,
        commits,
        truncated,
    })
}

pub(crate) fn versions() -> Value {
    json!({
        "engine": ENGINE_VERSION,
        "algorithm": ALGORITHM_VERSION,
        "line_survival": LINE_SURVIVAL_VERSION,
        "min_mixed_overlap": prov_capture::derive::MIN_MIXED_OVERLAP,
        "formula": FORMULA_VERSION,
        "eligibility": ELIGIBILITY_VERSION,
        "exclusion_classifier": prov_capture::exclusion::CLASSIFIER_ID,
        "coverage": COVERAGE_VERSION,
    })
}

pub(crate) fn created_at() -> String {
    iso8601_now()
}

struct CoverageDimensions {
    analysis: &'static str,
    content: &'static str,
    source: &'static str,
    history: &'static str,
}

fn coverage_dimensions(
    derived: usize,
    total: usize,
    eligible: usize,
    evidence: &BTreeMap<&str, usize>,
    evidence_complete: bool,
    tools: Option<&[String]>,
) -> CoverageDimensions {
    CoverageDimensions {
        analysis: analysis_coverage(derived, total),
        content: content_coverage(evidence, eligible, evidence_complete),
        source: source_coverage(tools),
        history: history_coverage(),
    }
}

// Analysis is complete only when every listed commit was derived; a nonempty partial
// derivation is partial, and a range with no commits has no analysis coverage.
fn analysis_coverage(derived: usize, total: usize) -> &'static str {
    if total == 0 {
        "unknown"
    } else if derived == total {
        "complete"
    } else if derived == 0 {
        "low"
    } else {
        "partial"
    }
}

// Content tiers require a strict majority of eligible lines, except that all-exact
// evidence is complete. A tie or unavailable evidence counts is unknown, not guessed.
fn content_coverage(
    evidence: &BTreeMap<&str, usize>,
    eligible: usize,
    evidence_complete: bool,
) -> &'static str {
    if eligible == 0 || !evidence_complete {
        return "unknown";
    }
    let exact = evidence.get("exact").copied().unwrap_or_default();
    let time_cut = evidence.get("time_cut").copied().unwrap_or_default();
    let reconstructed = evidence.get("reconstructed").copied().unwrap_or_default();
    let none = evidence.get("none").copied().unwrap_or_default();

    if exact == eligible {
        "complete"
    } else if exact.saturating_mul(2) > eligible {
        "high"
    } else if time_cut.saturating_add(reconstructed).saturating_mul(2) > eligible {
        "partial"
    } else if none.saturating_mul(2) > eligible {
        "low"
    } else {
        "unknown"
    }
}

// Source coverage only reports what stored tool metadata establishes; absent metadata
// is unknown, while a known empty tool set is low coverage.
fn source_coverage(tools: Option<&[String]>) -> &'static str {
    match tools {
        Some(tools) if !tools.is_empty() => "high",
        Some(_) => "low",
        None => "unknown",
    }
}

// Rewrite/rebase lineage is not yet verified by this report.
fn history_coverage() -> &'static str {
    "unknown"
}

fn resolve_revision(repo: &Path, revision: &str) -> Result<String, String> {
    let expression = format!("{revision}^{{commit}}");
    let output = git(
        repo,
        &["rev-parse", "--verify", "--end-of-options", &expression],
    )?;
    if !output.status.success() {
        return Err(format!("could not resolve Git revision {revision}"));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn resolve_merge_base(repo: &Path, base: &str, head: &str) -> Result<Option<String>, String> {
    let output = git(repo, &["merge-base", base, head])?;
    if !output.status.success() {
        return match output.status.code() {
            Some(1) => Ok(None),
            _ => Err("could not resolve Git merge base".into()),
        };
    }
    let merge_base = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    Ok((!merge_base.is_empty()).then_some(merge_base))
}

fn changed_files_count(repo: &Path, base: &str, head: &str) -> Result<usize, String> {
    let output = git(repo, &["diff", "-M", "--name-only", "-z", base, head])?;
    if !output.status.success() {
        return Err("could not read PR changed paths from Git".into());
    }
    Ok(output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .collect::<BTreeSet<_>>()
        .len())
}

fn git(repo: &Path, args: &[&str]) -> Result<std::process::Output, String> {
    Command::new("git")
        .args(args)
        .current_dir(repo)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .output()
        .map_err(|error| error.to_string())
}

fn iso8601_now() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let days = (seconds / 86_400) as i64;
    let mut year = 1970i64;
    let mut remaining_days = days;
    loop {
        let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
        let year_days = if leap { 366 } else { 365 };
        if remaining_days < year_days {
            break;
        }
        remaining_days -= year_days;
        year += 1;
    }
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let month_days = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut month = 1usize;
    for days_in_month in month_days {
        if remaining_days < days_in_month {
            break;
        }
        remaining_days -= days_in_month;
        month += 1;
    }
    let day = remaining_days + 1;
    let day_seconds = seconds % 86_400;
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        day_seconds / 3600,
        day_seconds % 3600 / 60,
        day_seconds % 60
    )
}

#[cfg(test)]
mod tests {
    use super::{
        analysis_coverage, content_coverage, coverage_band_for_range, coverage_dimensions,
        history_coverage, source_coverage, ALGORITHM_VERSION, COVERAGE_VERSION,
        ELIGIBILITY_VERSION, ENGINE_VERSION, FORMULA_VERSION, LINE_SURVIVAL_VERSION,
    };
    use std::collections::BTreeMap;

    #[test]
    fn versions_are_named_and_use_the_hook_package_version() {
        assert_eq!(ENGINE_VERSION, env!("CARGO_PKG_VERSION"));
        assert_eq!(ALGORITHM_VERSION, "derive-v1");
        assert_eq!(LINE_SURVIVAL_VERSION, "unobserved-position-mapped-v4");
        assert_eq!(FORMULA_VERSION, "strict-v1");
        assert_eq!(ELIGIBILITY_VERSION, "non-blank-added-v1");
        assert_eq!(COVERAGE_VERSION, "coverage-v3");
    }

    #[test]
    fn analysis_coverage_handles_empty_complete_partial_and_missing_reports() {
        assert_eq!(analysis_coverage(0, 0), "unknown");
        assert_eq!(analysis_coverage(3, 3), "complete");
        assert_eq!(analysis_coverage(0, 3), "low");
        assert_eq!(analysis_coverage(1, 3), "partial");
    }

    #[test]
    fn one_commit_pr_coverage_preserves_the_commit_band() {
        assert_eq!(
            coverage_band_for_range(true, 1, Some("partial"), 10, 10, 0),
            serde_json::json!("partial")
        );
        assert_eq!(
            coverage_band_for_range(true, 2, None, 20, 20, 0),
            serde_json::json!("high")
        );
    }

    #[test]
    fn content_coverage_uses_eligible_line_majorities() {
        assert_eq!(content_coverage(&BTreeMap::new(), 0, true), "unknown");
        assert_eq!(
            content_coverage(&BTreeMap::from([("exact", 10)]), 10, true),
            "complete"
        );
        assert_eq!(
            content_coverage(&BTreeMap::from([("exact", 6), ("none", 4)]), 10, true),
            "high"
        );
        assert_eq!(
            content_coverage(
                &BTreeMap::from([("time_cut", 3), ("reconstructed", 3), ("exact", 4)]),
                10,
                true
            ),
            "partial"
        );
        assert_eq!(
            content_coverage(&BTreeMap::from([("none", 6), ("exact", 4)]), 10, true),
            "low"
        );
        assert_eq!(
            content_coverage(&BTreeMap::from([("exact", 5), ("none", 5)]), 10, true),
            "unknown"
        );
        assert_eq!(
            content_coverage(&BTreeMap::from([("exact", 10)]), 10, false),
            "unknown"
        );
    }

    #[test]
    fn source_coverage_distinguishes_seen_empty_and_unavailable_tools() {
        assert_eq!(source_coverage(Some(&["codex".to_owned()])), "high");
        assert_eq!(source_coverage(Some(&[])), "low");
        assert_eq!(source_coverage(None), "unknown");
    }

    #[test]
    fn source_coverage_uses_observed_tools_when_analysis_is_partial() {
        let dimensions = coverage_dimensions(
            1,
            2,
            10,
            &BTreeMap::from([("exact", 10)]),
            false,
            Some(&["codex".to_owned()]),
        );
        assert_eq!(dimensions.analysis, "partial");
        assert_eq!(dimensions.content, "unknown");
        assert_eq!(dimensions.source, "high");
        assert_eq!(dimensions.history, "unknown");
    }

    #[test]
    fn history_coverage_remains_unknown() {
        assert_eq!(history_coverage(), "unknown");
    }
}
