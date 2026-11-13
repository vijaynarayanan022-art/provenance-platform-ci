use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

struct Options {
    repo: Option<PathBuf>,
    commit: Option<String>,
    range: Option<String>,
    last: Option<usize>,
    file: Option<String>,
    format: String,
}

struct Row {
    commit: String,
    status: &'static str,
    files: Option<usize>,
    eligible: Option<usize>,
    ai_direct: Option<usize>,
    ai_strong: Option<usize>,
    ai_probable: Option<usize>,
    mixed: Option<usize>,
    unknown: Option<usize>,
    excluded: Option<usize>,
    deterministic_pct: Option<f64>,
    linked_pct: Option<f64>,
    coverage: Option<String>,
    coverage_unobserved_lines: Option<usize>,
    tools: Option<Vec<String>>,
    report: Option<Value>,
}

struct Totals {
    status: &'static str,
    files: Option<usize>,
    eligible: Option<usize>,
    ai_direct: Option<usize>,
    ai_strong: Option<usize>,
    ai_probable: Option<usize>,
    mixed: Option<usize>,
    unknown: Option<usize>,
    excluded: Option<usize>,
    deterministic_pct: Option<f64>,
    linked_pct: Option<f64>,
    coverage: Option<String>,
    tools: Option<Vec<String>>,
}

struct FileRange {
    commit: String,
    file: String,
    start_line: usize,
    end_line: usize,
    supporting_basis: Vec<String>,
    limiting_basis: Vec<String>,
    algorithm_profile: String,
    score_profile: String,
    category: String,
    score: Option<f64>,
    confidence_band: String,
    primary_basis: Option<String>,
    adapter_id: Option<String>,
    tool_id: Option<String>,
    session_ref: Option<String>,
    operation: Option<String>,
    exclusion_classifier: Option<String>,
    exclusion_reason: Option<String>,
    prior_ai_origin: Option<Value>,
}

pub fn command(args: impl Iterator<Item = String>) -> Result<String, String> {
    let options = parse_options(args)?;
    let path = options
        .repo
        .clone()
        .or_else(|| std::env::current_dir().ok())
        .ok_or("could not determine current directory")?;
    let repo = git_hooks::repo_root(&path)?;
    let commit_ids = select_commits(&repo, &options)?;
    let reports = prov_capture::commit::reports_for_commits(&repo, &commit_ids)?;
    let line_sidecars = prov_capture::commit::lines_for_commits(&repo, &commit_ids)?;
    let mut rows = Vec::with_capacity(commit_ids.len());
    let mut tool_commit_ids = BTreeSet::new();
    for ((commit, report), sidecar) in commit_ids.iter().zip(reports.reports).zip(line_sidecars) {
        let sidecar_unobserved_lines = sidecar
            .as_deref()
            .map(prov_capture::commit::unobserved_eligible_lines_from_sidecar);
        let row = make_row(commit, report.as_deref(), sidecar_unobserved_lines);
        if let Some(report) = &row.report {
            if let Some(own_commit) = report["commit"].as_str() {
                tool_commit_ids.insert(own_commit.to_owned());
            }
            if let Some(sources) = report["rewrite"]["sources"].as_array() {
                tool_commit_ids.extend(sources.iter().filter_map(Value::as_str).map(str::to_owned));
            }
        }
        rows.push(row);
    }
    let tool_commit_ids: Vec<_> = tool_commit_ids.into_iter().collect();
    let observed_tools = prov_capture::commit::tools_for_commits(&repo, &tool_commit_ids)
        .ok()
        .map(|per_commit| {
            tool_commit_ids
                .iter()
                .zip(per_commit)
                .map(|(commit, tools)| (commit.clone(), tools.into_iter().collect::<BTreeSet<_>>()))
                .collect::<BTreeMap<_, _>>()
        });
    for row in &mut rows {
        if let Some(report) = &row.report {
            let mut ids = BTreeSet::new();
            if let Some(commit) = report["commit"].as_str() {
                ids.insert(commit.to_owned());
            }
            if let Some(sources) = report["rewrite"]["sources"].as_array() {
                ids.extend(sources.iter().filter_map(Value::as_str).map(str::to_owned));
            }
            row.tools = observed_tools.as_ref().map(|observed| {
                ids.iter()
                    .filter_map(|id| observed.get(id))
                    .flat_map(|tools| tools.iter().cloned())
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect()
            });
        }
    }

    let range = options.range.as_deref().map(parse_range).transpose()?;
    let (totals, truncated) = if let Some((base, head)) = range {
        let range_report = pr_report_for_range(&repo, &base, &head)?;
        let truncated = range_report["commits"]["truncated"]
            .as_bool()
            .unwrap_or(false);
        (totals_from_pr_report(&range_report), truncated)
    } else {
        (totals_from_rows(&rows), false)
    };
    let file_ranges = match options.file.as_deref() {
        Some(file) => {
            let relative = repo_relative_file(&repo, file)
                .ok_or_else(|| format!("file {file} is outside the repository"))?;
            let included = rows.iter().any(|row| {
                row.report
                    .as_ref()
                    .and_then(|report| report["files"].as_array())
                    .is_some_and(|files| {
                        files
                            .iter()
                            .any(|item| item["path"].as_str() == Some(&relative))
                    })
            });
            if !included {
                return Err(format!(
                    "file {relative} is not present in the selected commit(s)"
                ));
            }
            collect_file_ranges(&repo, &commit_ids, file)?
        }
        None => Vec::new(),
    };
    if options.format == "json" {
        let repo_id = reports.repo_id;
        let output = json!({
            "schema": "prov.summary/0.1",
            "created_at": pr_report::created_at(),
            "versions": pr_report::versions(),
            "repo_id": repo_id,
            "commits": rows.iter().map(row_json).collect::<Vec<_>>(),
            "totals": totals_json(&totals),
            "truncated": truncated,
            "file_ranges": file_ranges.iter().map(file_range_json).collect::<Vec<_>>(),
        });
        return serde_json::to_string(&output).map_err(|error| error.to_string());
    }
    Ok(render_table(&rows, &totals, &file_ranges))
}

fn parse_options(args: impl Iterator<Item = String>) -> Result<Options, String> {
    let mut options = Options {
        repo: None,
        commit: None,
        range: None,
        last: None,
        file: None,
        format: "table".into(),
    };
    let mut args = args;
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--repo" => {
                options.repo = Some(PathBuf::from(args.next().ok_or("--repo needs a path")?))
            }
            "--commit" => options.commit = Some(args.next().ok_or("--commit needs a revision")?),
            "--range" => options.range = Some(args.next().ok_or("--range needs A..B")?),
            "--last" => {
                options.last = Some(
                    args.next()
                        .ok_or("--last needs a count")?
                        .parse()
                        .map_err(|_| "--last must be a non-negative integer")?,
                )
            }
            "--file" => options.file = Some(args.next().ok_or("--file needs a path")?),
            "--format" => options.format = args.next().ok_or("--format needs table or json")?,
            _ => return Err(format!("unknown summary argument: {argument}")),
        }
    }
    let selections = usize::from(options.commit.is_some())
        + usize::from(options.range.is_some())
        + usize::from(options.last.is_some());
    if selections > 1 {
        return Err("--commit, --range, and --last are mutually exclusive".into());
    }
    if !matches!(options.format.as_str(), "table" | "json") {
        return Err("--format must be table or json".into());
    }
    Ok(options)
}

fn select_commits(repo: &Path, options: &Options) -> Result<Vec<String>, String> {
    if let Some(revision) = &options.commit {
        return Ok(vec![resolve_commit(repo, revision)?]);
    }
    if let Some(range) = &options.range {
        let (base, head) = parse_range(range)?;
        return Ok(pr_report::resolve_commit_range(repo, &base, &head)?.commits);
    }
    latest_derived(repo, options.last.unwrap_or(10))
}

fn latest_derived(repo: &Path, count: usize) -> Result<Vec<String>, String> {
    let mut selected = Vec::with_capacity(count);
    let mut skip = 0usize;
    const BATCH: usize = 100;
    while selected.len() < count {
        let skip_arg = format!("--skip={skip}");
        let output = git(
            repo,
            &[
                "rev-list",
                "--first-parent",
                "--no-merges",
                &skip_arg,
                "--max-count=100",
                "HEAD",
            ],
        )?;
        if !output.status.success() {
            return Err("could not list current branch commits".into());
        }
        let batch: Vec<String> = String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::to_owned)
            .collect();
        if batch.is_empty() {
            break;
        }
        let reports = prov_capture::commit::reports_for_commits(repo, &batch)?.reports;
        for (commit, report) in batch.iter().zip(reports) {
            if report.is_some() {
                selected.push(commit.clone());
                if selected.len() == count {
                    break;
                }
            }
        }
        skip += batch.len().min(BATCH);
        if batch.len() < BATCH {
            break;
        }
    }
    Ok(selected)
}

fn resolve_commit(repo: &Path, revision: &str) -> Result<String, String> {
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

fn parse_range(range: &str) -> Result<(String, String), String> {
    let Some((base, head)) = range.split_once("..") else {
        return Err("range must use A..B syntax".into());
    };
    if base.trim().is_empty() || head.trim().is_empty() || head.contains("..") {
        return Err("range must use A..B syntax".into());
    }
    Ok((base.to_owned(), head.to_owned()))
}

fn pr_report_for_range(repo: &Path, base: &str, head: &str) -> Result<Value, String> {
    let args = vec![
        "--repo".into(),
        repo.to_string_lossy().into_owned(),
        "--base".into(),
        base.into(),
        "--head".into(),
        head.into(),
        "--no-derive".into(),
    ];
    let (bytes, _) = pr_report::command(args.into_iter())?;
    serde_json::from_slice(&bytes).map_err(|error| error.to_string())
}

fn make_row(
    commit: &str,
    report_bytes: Option<&[u8]>,
    sidecar_unobserved_lines: Option<usize>,
) -> Row {
    let Some(report) = report_bytes.and_then(|bytes| serde_json::from_slice::<Value>(bytes).ok())
    else {
        return Row::not_derived(commit);
    };
    if report["schema"] != "prov.commit/0.1" {
        return Row::not_derived(commit);
    }
    if report.get("skipped").is_some() {
        return Row {
            commit: commit.into(),
            status: "skipped",
            files: Some(0),
            eligible: None,
            ai_direct: None,
            ai_strong: None,
            ai_probable: None,
            mixed: None,
            unknown: None,
            excluded: None,
            deterministic_pct: None,
            linked_pct: None,
            coverage: None,
            coverage_unobserved_lines: None,
            tools: None,
            report: Some(report),
        };
    }
    let totals = &report["totals"];
    let Some(eligible) = number(totals, "e") else {
        return Row::not_derived(commit);
    };
    let (
        Some(ai_direct),
        Some(ai_strong),
        Some(ai_probable),
        Some(mixed),
        Some(unknown),
        Some(excluded),
    ) = (
        number(totals, "ai_direct"),
        number(totals, "ai_strong"),
        number(totals, "ai_probable"),
        number(totals, "mixed"),
        number(totals, "unknown"),
        number(totals, "excluded"),
    )
    else {
        return Row::not_derived(commit);
    };
    let coverage_unobserved_lines = report["coverage_unobserved_lines"]
        .as_u64()
        .map(|value| value as usize)
        .or(sidecar_unobserved_lines)
        .unwrap_or_default();
    let coverage = report["coverage"].as_str().map(|band| {
        prov_capture::commit::coverage_band_limited(band, eligible, coverage_unobserved_lines)
            .to_owned()
    });
    Row {
        commit: commit.into(),
        status: "derived",
        files: report["files"].as_array().map(Vec::len),
        eligible: Some(eligible),
        ai_direct: Some(ai_direct),
        ai_strong: Some(ai_strong),
        ai_probable: Some(ai_probable),
        mixed: Some(mixed),
        unknown: Some(unknown),
        excluded: Some(excluded),
        deterministic_pct: optional_number(totals, "ai_deterministic_pct"),
        linked_pct: optional_number(totals, "ai_linked_pct"),
        coverage,
        coverage_unobserved_lines: Some(coverage_unobserved_lines),
        tools: None,
        report: Some(report),
    }
}

impl Row {
    fn not_derived(commit: &str) -> Self {
        Self {
            commit: commit.into(),
            status: "not derived",
            files: None,
            eligible: None,
            ai_direct: None,
            ai_strong: None,
            ai_probable: None,
            mixed: None,
            unknown: None,
            excluded: None,
            deterministic_pct: None,
            linked_pct: None,
            coverage: None,
            coverage_unobserved_lines: None,
            tools: None,
            report: None,
        }
    }
}

fn number(value: &Value, field: &str) -> Option<usize> {
    value.get(field)?.as_u64().map(|number| number as usize)
}

fn optional_number(value: &Value, field: &str) -> Option<f64> {
    value.get(field)?.as_f64()
}

fn totals_from_pr_report(report: &Value) -> Totals {
    let lines = &report["lines"];
    let complete = report["commits"]["underived"].as_u64() == Some(0)
        && report["commits"]["truncated"].as_bool() == Some(false)
        && lines["eligible"].as_u64().is_some();
    let metric = |field: &str| complete.then(|| number(lines, field)).flatten();
    let tools = complete
        .then(|| {
            report["tools"].as_array().map(|values| {
                values
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
        })
        .flatten();
    Totals {
        status: if complete { "derived" } else { "not derived" },
        files: report["files_changed"].as_u64().map(|value| value as usize),
        eligible: metric("eligible"),
        ai_direct: metric("ai_direct"),
        ai_strong: metric("ai_strong"),
        ai_probable: metric("ai_probable"),
        mixed: metric("mixed"),
        unknown: metric("unknown"),
        excluded: metric("excluded"),
        deterministic_pct: complete
            .then(|| report["ai_deterministic_pct"].as_f64())
            .flatten(),
        linked_pct: complete.then(|| report["ai_linked_pct"].as_f64()).flatten(),
        coverage: complete
            .then(|| report["coverage"]["band"].as_str().map(str::to_owned))
            .flatten(),
        tools,
    }
}

fn totals_from_rows(rows: &[Row]) -> Totals {
    if rows.iter().any(|row| row.status != "derived") {
        return Totals::not_derived();
    }
    let mut file_paths = BTreeSet::new();
    let mut eligible = 0usize;
    let mut ai_direct = 0usize;
    let mut ai_strong = 0usize;
    let mut ai_probable = 0usize;
    let mut mixed = 0usize;
    let mut unknown = 0usize;
    let mut excluded = 0usize;
    let mut covered = 0usize;
    let mut unobserved_lines = 0usize;
    let mut tools = BTreeSet::new();
    for row in rows {
        eligible += row.eligible.unwrap_or_default();
        ai_direct += row.ai_direct.unwrap_or_default();
        ai_strong += row.ai_strong.unwrap_or_default();
        ai_probable += row.ai_probable.unwrap_or_default();
        mixed += row.mixed.unwrap_or_default();
        unknown += row.unknown.unwrap_or_default();
        excluded += row.excluded.unwrap_or_default();
        unobserved_lines += row.coverage_unobserved_lines.unwrap_or_default();
        if let Some(values) = &row.tools {
            tools.extend(values.iter().cloned());
        }
        if let Some(files) = row
            .report
            .as_ref()
            .and_then(|report| report["files"].as_array())
        {
            for file in files {
                if let Some(path) = file["path"].as_str() {
                    file_paths.insert(path.to_owned());
                }
                if !matches!(file["evidence"].as_str(), Some("none" | "missing_blob")) {
                    covered += number(file, "e").unwrap_or_default();
                }
            }
        }
    }
    Totals {
        status: "derived",
        files: Some(file_paths.len()),
        eligible: Some(eligible),
        ai_direct: Some(ai_direct),
        ai_strong: Some(ai_strong),
        ai_probable: Some(ai_probable),
        mixed: Some(mixed),
        unknown: Some(unknown),
        excluded: Some(excluded),
        deterministic_pct: prov_capture::commit::percentage_tenths(ai_direct + ai_strong, eligible),
        linked_pct: prov_capture::commit::percentage_tenths(
            ai_direct + ai_strong + ai_probable + mixed,
            eligible,
        ),
        coverage: Some(if rows.len() == 1 {
            rows[0].coverage.clone().unwrap_or_else(|| "unknown".into())
        } else {
            prov_capture::commit::coverage_band_with_unobserved(covered, eligible, unobserved_lines)
                .into()
        }),
        tools: Some(tools.into_iter().collect()),
    }
}

impl Totals {
    fn not_derived() -> Self {
        Self {
            status: "not derived",
            files: None,
            eligible: None,
            ai_direct: None,
            ai_strong: None,
            ai_probable: None,
            mixed: None,
            unknown: None,
            excluded: None,
            deterministic_pct: None,
            linked_pct: None,
            coverage: None,
            tools: None,
        }
    }
}

fn row_json(row: &Row) -> Value {
    json!({
        "commit": row.commit,
        "status": row.status,
        "files": row.files,
        "eligible": row.eligible,
        "ai_direct": row.ai_direct,
        "ai_strong": row.ai_strong,
        "ai_probable": row.ai_probable,
        "mixed": row.mixed,
        "unknown": row.unknown,
        "excluded": row.excluded,
        "deterministic_pct": row.deterministic_pct,
        "linked_pct": row.linked_pct,
        "coverage": row.coverage,
        "tools": row.tools,
    })
}

fn totals_json(totals: &Totals) -> Value {
    json!({
        "status": totals.status,
        "files": totals.files,
        "eligible": totals.eligible,
        "ai_direct": totals.ai_direct,
        "ai_strong": totals.ai_strong,
        "ai_probable": totals.ai_probable,
        "mixed": totals.mixed,
        "unknown": totals.unknown,
        "excluded": totals.excluded,
        "deterministic_pct": totals.deterministic_pct,
        "linked_pct": totals.linked_pct,
        "coverage": totals.coverage,
        "tools": totals.tools,
    })
}

fn render_table(rows: &[Row], totals: &Totals, file_ranges: &[FileRange]) -> String {
    const HEADERS: [&str; 13] = [
        "commit",
        "files",
        "eligible",
        "ai_direct",
        "ai_strong",
        "ai_probable",
        "mixed",
        "unknown",
        "excluded",
        "deterministic_pct",
        "linked_pct",
        "coverage",
        "tools",
    ];
    let mut output = format!("{}\n", HEADERS.join("\t"));
    for row in rows {
        output.push_str(&format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
            row.commit,
            cell(row.files, row.status),
            cell(row.eligible, row.status),
            cell(row.ai_direct, row.status),
            cell(row.ai_strong, row.status),
            cell(row.ai_probable, row.status),
            cell(row.mixed, row.status),
            cell(row.unknown, row.status),
            cell(row.excluded, row.status),
            percent(row.deterministic_pct, row.status),
            percent(row.linked_pct, row.status),
            row.coverage.as_deref().unwrap_or(row.status),
            tools_cell(row.tools.as_deref(), row.status),
        ));
    }
    output.push_str(&format!(
        "TOTAL\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
        cell(totals.files, totals.status),
        cell(totals.eligible, totals.status),
        cell(totals.ai_direct, totals.status),
        cell(totals.ai_strong, totals.status),
        cell(totals.ai_probable, totals.status),
        cell(totals.mixed, totals.status),
        cell(totals.unknown, totals.status),
        cell(totals.excluded, totals.status),
        percent(totals.deterministic_pct, totals.status),
        percent(totals.linked_pct, totals.status),
        totals.coverage.as_deref().unwrap_or(totals.status),
        tools_cell(totals.tools.as_deref(), totals.status),
    ));
    if !file_ranges.is_empty() {
        output.push_str(
            "\ncommit\tfile\tlines\tcategory\tscore\tband\tprimaryBasis\tsupportingBasis\tlimitingBasis\tsourceTool\toperation\talgorithmProfile\tscoreProfile\texclusionClassifier\texclusionReason\n",
        );
        for range in file_ranges {
            output.push_str(&format!(
                "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
                range.commit,
                range.file,
                range_line_span(range.start_line, range.end_line),
                range.category,
                range
                    .score
                    .map_or_else(|| "n/a".into(), |score| format!("{score:.2}")),
                range.confidence_band,
                range.primary_basis.as_deref().unwrap_or("none"),
                range.supporting_basis.join(","),
                range.limiting_basis.join(","),
                source_tool(range),
                range.operation.as_deref().unwrap_or("none"),
                range.algorithm_profile,
                range.score_profile,
                range.exclusion_classifier.as_deref().unwrap_or("none"),
                range.exclusion_reason.as_deref().unwrap_or("none"),
            ));
        }
    }
    output
}

fn cell(value: Option<usize>, status: &str) -> String {
    value.map_or_else(|| status.into(), |value| value.to_string())
}

fn percent(value: Option<f64>, status: &str) -> String {
    value.map_or_else(
        || {
            if status == "derived" {
                "n/a".into()
            } else {
                status.into()
            }
        },
        |value| format!("{value:.1}%"),
    )
}

fn tools_cell(tools: Option<&[String]>, status: &str) -> String {
    tools.map_or_else(
        || status.into(),
        |tools| {
            if tools.is_empty() {
                "none".into()
            } else {
                tools.join(",")
            }
        },
    )
}

fn collect_file_ranges(
    repo: &Path,
    commits: &[String],
    requested: &str,
) -> Result<Vec<FileRange>, String> {
    let path = repo_relative_file(repo, requested)
        .ok_or_else(|| format!("file {requested} is outside the repository"))?;
    collect_file_ranges_filtered(repo, commits, Some(&path))
}

pub(crate) fn collect_all_file_ranges(
    repo: &Path,
    commits: &[String],
) -> Result<Vec<Value>, String> {
    collect_file_ranges_filtered(repo, commits, None)
        .map(|ranges| ranges.iter().map(file_range_json).collect())
}

fn collect_file_ranges_filtered(
    repo: &Path,
    commits: &[String],
    requested: Option<&str>,
) -> Result<Vec<FileRange>, String> {
    let sidecars = prov_capture::commit::lines_for_commits(repo, commits)?;
    let mut ranges = Vec::new();
    for (commit, bytes) in commits.iter().zip(sidecars) {
        let Some(bytes) = bytes else { continue };
        let Ok(document) = serde_json::from_slice::<Value>(&bytes) else {
            continue;
        };
        if let Some(file_attributions) = document["attributions"].as_array() {
            for attribution in file_attributions
                .iter()
                .filter(|range| requested.is_none_or(|path| range["path"].as_str() == Some(path)))
            {
                let Some(path) = attribution["path"].as_str() else {
                    continue;
                };
                let Some(start_line) = attribution["startLine"].as_u64() else {
                    continue;
                };
                let Some(end_line) = attribution["endLine"].as_u64() else {
                    continue;
                };
                ranges.push(FileRange {
                    commit: commit.clone(),
                    file: path.to_owned(),
                    start_line: start_line as usize,
                    end_line: end_line as usize,
                    supporting_basis: attribution["supportingBasis"]
                        .as_array()
                        .map(|items| {
                            items
                                .iter()
                                .filter_map(Value::as_str)
                                .map(str::to_owned)
                                .collect()
                        })
                        .unwrap_or_default(),
                    limiting_basis: attribution["limitingBasis"]
                        .as_array()
                        .map(|items| {
                            items
                                .iter()
                                .filter_map(Value::as_str)
                                .map(str::to_owned)
                                .collect()
                        })
                        .unwrap_or_default(),
                    algorithm_profile: attribution["algorithmProfile"]
                        .as_str()
                        .unwrap_or("deterministic-v1")
                        .to_owned(),
                    score_profile: attribution["scoreProfile"]
                        .as_str()
                        .unwrap_or("score-v1")
                        .to_owned(),
                    category: attribution["category"]
                        .as_str()
                        .unwrap_or("unknown")
                        .to_owned(),
                    score: attribution["score"].as_f64(),
                    confidence_band: attribution["confidenceBand"]
                        .as_str()
                        .unwrap_or("none")
                        .to_owned(),
                    primary_basis: attribution["primaryBasis"].as_str().map(str::to_owned),
                    adapter_id: attribution["source"]["adapterId"]
                        .as_str()
                        .map(str::to_owned),
                    tool_id: attribution["source"]["toolId"].as_str().map(str::to_owned),
                    session_ref: attribution["source"]["sessionRef"]
                        .as_str()
                        .map(str::to_owned),
                    operation: attribution["operation"].as_str().map(str::to_owned),
                    exclusion_classifier: attribution["exclusionClassifier"]
                        .as_str()
                        .map(str::to_owned),
                    exclusion_reason: attribution["exclusionReason"].as_str().map(str::to_owned),
                    prior_ai_origin: attribution
                        .get("priorAiOrigin")
                        .filter(|origin| origin.is_object())
                        .cloned(),
                });
            }
        } else if let Some(file_rows) = document["files"].as_array() {
            for file in file_rows
                .iter()
                .filter(|file| requested.is_none_or(|path| file["path"].as_str() == Some(path)))
            {
                let Some(path) = file["path"].as_str() else {
                    continue;
                };
                if let Some(file_lines) = file["lines"].as_array() {
                    for line in file_lines {
                        let Some(number) = line["line"].as_u64() else {
                            continue;
                        };
                        let label = line["label"]
                            .as_str()
                            .and_then(|label| label.chars().next())
                            .or_else(|| line["label"].as_u64().map(|value| value as u8 as char));
                        ranges.push(FileRange {
                            commit: commit.clone(),
                            file: path.to_owned(),
                            start_line: number as usize,
                            end_line: number as usize,
                            category: full_category(label.unwrap_or('u')).into(),
                            score: None,
                            confidence_band: "none".into(),
                            primary_basis: None,
                            supporting_basis: Vec::new(),
                            limiting_basis: Vec::new(),
                            algorithm_profile: "deterministic-v1".into(),
                            score_profile: "score-v1".into(),
                            adapter_id: None,
                            tool_id: None,
                            session_ref: None,
                            operation: None,
                            exclusion_classifier: None,
                            exclusion_reason: None,
                            prior_ai_origin: None,
                        });
                    }
                }
            }
        }
    }
    Ok(ranges)
}

fn full_category(label: char) -> &'static str {
    match label {
        'd' => "ai_direct",
        's' => "ai_strong",
        'p' => "ai_probable",
        'm' => "mixed",
        'x' => "excluded",
        _ => "unknown",
    }
}

fn file_range_json(range: &FileRange) -> Value {
    json!({
        "commit": range.commit,
        "file": range.file,
        "startLine": range.start_line,
        "endLine": range.end_line,
        "endLineExclusive": range.end_line + 1,
        "lines": range.end_line.saturating_sub(range.start_line) + 1,
        "category": range.category,
        "score": range.score,
        "confidenceBand": range.confidence_band,
        "primaryBasis": range.primary_basis,
        "supportingBasis": range.supporting_basis,
        "limitingBasis": range.limiting_basis,
        "source": {
            "adapterId": range.adapter_id,
            "toolId": range.tool_id,
            "sessionRef": range.session_ref,
        },
        "sourceTool": source_tool(range),
        "operation": range.operation,
        "priorAiOrigin": range.prior_ai_origin,
        "algorithmProfile": range.algorithm_profile,
        "scoreProfile": range.score_profile,
        "exclusionClassifier": range.exclusion_classifier,
        "exclusionReason": range.exclusion_reason,
    })
}

fn range_line_span(start: usize, end: usize) -> String {
    if start == end {
        start.to_string()
    } else {
        format!("{start}-{end}")
    }
}

fn source_tool(range: &FileRange) -> String {
    range
        .tool_id
        .as_deref()
        .or(range.adapter_id.as_deref())
        .unwrap_or("none")
        .into()
}

fn repo_relative_file(repo: &Path, requested: &str) -> Option<String> {
    let path = PathBuf::from(requested.replace('\\', "/"));
    let path = if path.is_absolute() {
        path
    } else {
        repo.join(path)
    };
    let normalized = canonical_or_lexical(&path);
    let root = canonical_or_lexical(repo);
    let relative = normalized.strip_prefix(root).ok()?;
    let relative = relative.to_string_lossy().replace('\\', "/");
    (!relative.is_empty() && relative != "." && !relative.starts_with("../")).then_some(relative)
}

fn canonical_or_lexical(path: &Path) -> PathBuf {
    let normalized = lexical_normalize(path);
    let mut ancestor = normalized.as_path();
    let mut missing = Vec::new();
    loop {
        if let Ok(mut resolved) = ancestor.canonicalize() {
            for component in missing.iter().rev() {
                resolved.push(component);
            }
            return resolved;
        }
        let Some(name) = ancestor.file_name() else {
            return normalized;
        };
        missing.push(name.to_os_string());
        let Some(parent) = ancestor.parent() else {
            return normalized;
        };
        ancestor = parent;
    }
}

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut components = Vec::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                components.pop();
            }
            other => components.push(other.as_os_str().to_owned()),
        }
    }
    components.into_iter().collect()
}

fn git(repo: &Path, args: &[&str]) -> Result<std::process::Output, String> {
    let mut command = Command::new("git");
    command
        .args(args)
        .current_dir(repo)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null());
    prov_capture::configure_process_command(&mut command, false);
    command.output().map_err(|error| error.to_string())
}

use crate::{git_hooks, pr_report};

#[cfg(test)]
mod tests {
    use super::{
        full_category, render_table, row_json, totals_from_rows, totals_json, FileRange, Row,
    };

    #[test]
    fn line_codes_expand_to_full_category_names() {
        for (code, expected) in [
            ('d', "ai_direct"),
            ('s', "ai_strong"),
            ('p', "ai_probable"),
            ('m', "mixed"),
            ('u', "unknown"),
            ('x', "excluded"),
        ] {
            assert_eq!(full_category(code), expected);
        }
    }

    #[test]
    fn single_commit_total_uses_the_commit_coverage_band() {
        let row = Row {
            commit: "commit".into(),
            status: "derived",
            files: Some(1),
            eligible: Some(10),
            ai_direct: Some(10),
            ai_strong: Some(0),
            ai_probable: Some(0),
            mixed: Some(0),
            unknown: Some(0),
            excluded: Some(0),
            deterministic_pct: Some(100.0),
            linked_pct: Some(100.0),
            coverage: Some("partial".into()),
            coverage_unobserved_lines: Some(0),
            tools: Some(Vec::new()),
            report: Some(
                serde_json::json!({"files":[{"path":"file.txt","e":10,"evidence":"exact"}]}),
            ),
        };
        let rows = vec![row];
        let totals = totals_from_rows(&rows);
        assert_eq!(totals.coverage.as_deref(), Some("partial"));
        assert_eq!(row_json(&rows[0])["coverage"], "partial");
        assert_eq!(totals_json(&totals)["coverage"], "partial");
        let table = render_table(&rows, &totals, &[] as &[FileRange]);
        let coverage_cells: Vec<_> = table
            .lines()
            .skip(1)
            .map(|line| line.split('\t').nth(11).unwrap())
            .collect();
        assert_eq!(coverage_cells, ["partial", "partial"]);
    }
}
