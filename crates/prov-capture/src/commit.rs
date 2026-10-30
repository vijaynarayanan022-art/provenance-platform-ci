use crate::derive::{self, Evidence, Step, StepKind};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const MAX_FILE: usize = 1024 * 1024;
const NANOSECOND: u128 = 1_000_000_000;

#[derive(Serialize)]
struct CommitOutput {
    schema: &'static str,
    repo_id: String,
    commit: String,
    parent: Option<String>,
    totals: Totals,
    coverage: &'static str,
    files: Vec<FileOutput>,
}

#[derive(Serialize)]
struct MergeOutput {
    schema: &'static str,
    commit: String,
    skipped: &'static str,
}

#[derive(Clone, Copy, Default, Serialize)]
struct Totals {
    e: usize,
    ai_direct: usize,
    ai_strong: usize,
    ai_probable: usize,
    mixed: usize,
    unknown: usize,
    excluded: usize,
    ai_deterministic_pct: Option<f64>,
    ai_linked_pct: Option<f64>,
}

#[derive(Serialize)]
struct FileOutput {
    path: String,
    e: usize,
    ai_strong: usize,
    ai_probable: usize,
    mixed: usize,
    unknown: usize,
    excluded: usize,
    evidence: &'static str,
}

#[derive(Clone)]
struct CommitFile {
    path: String,
    old_blob: Option<String>,
    new_blob: String,
}

#[derive(Clone, Deserialize)]
struct EventRecord {
    event: String,
    timestamp: u128,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    after_sha: Option<String>,
    #[serde(default)]
    degraded: Option<bool>,
    #[serde(default)]
    overlap: Option<bool>,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    new_head: Option<String>,
    #[serde(default)]
    commit_in_window: Option<bool>,
    #[serde(default)]
    paths: Option<Vec<String>>,
    #[serde(default)]
    truncated: Option<bool>,
    #[serde(skip)]
    order: usize,
}

pub fn derive_commit(repo_path: &Path, revision: &str) -> Result<PathBuf, String> {
    let (repo, repo_id, store) = repo_context(repo_path)?;
    let commit = resolve_commit(&repo.root, revision)?;
    let parent_line = git_checked(&repo.root, &["rev-list", "--parents", "-n", "1", &commit])?;
    let commits: Vec<_> = parent_line.split_whitespace().collect();
    if commits.first() != Some(&commit.as_str()) {
        return Err("could not read commit ancestry".into());
    }
    let parents = &commits[1..];
    let output_path = store.join("commits").join(format!("{commit}.json"));

    if parents.len() > 1 {
        let bytes = serde_json::to_vec(&MergeOutput {
            schema: "prov.commit/0.1",
            commit,
            skipped: "merge_commit",
        })
        .map_err(|error| error.to_string())?;
        crate::ensure_private_dir(output_path.parent().ok_or("invalid commit path")?)
            .map_err(|error| error.to_string())?;
        crate::atomic_bytes(&output_path, &bytes, false).map_err(|error| error.to_string())?;
        return Ok(output_path);
    }

    let parent = parents.first().map(|value| (*value).to_owned());
    let parent_time = parent
        .as_deref()
        .map(|value| committer_time_ns(&repo.root, value))
        .transpose()?
        .unwrap_or(0);
    let commit_time = committer_time_ns(&repo.root, &commit)?;
    let files = commit_files(&repo.root, parent.as_deref(), &commit)?;
    let mut blob_ids = BTreeSet::new();
    for file in &files {
        blob_ids.insert(file.new_blob.clone());
        if let Some(old) = &file.old_blob {
            blob_ids.insert(old.clone());
        }
    }
    let ids: Vec<_> = blob_ids.into_iter().collect();
    let sizes = crate::check_blob_sizes(&repo.root, &ids);
    let available_ids: Vec<_> = sizes
        .as_ref()
        .map(|sizes| {
            ids.iter()
                .filter(|id| sizes.contains_key(*id))
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    let contents = crate::cat_blobs(&repo.root, &available_ids);
    let event_records = read_events(&store.join("events"));
    let head_moves = event_records
        .iter()
        .filter(|event| {
            event.event == "head_move"
                && event.new_head.as_deref() == Some(&commit)
                && event.commit_in_window == Some(true)
        })
        .collect::<Vec<_>>();
    let reconstruct_all = head_moves.iter().any(|event| event.truncated == Some(true));
    let reconstruct_paths: BTreeSet<&str> = head_moves
        .iter()
        .flat_map(|event| event.paths.iter().flatten().map(String::as_str))
        .collect();

    let mut output_files = Vec::with_capacity(files.len());
    for file in files {
        let old_size = file
            .old_blob
            .as_ref()
            .and_then(|id| sizes.as_ref()?.get(id))
            .copied();
        let new_size = sizes
            .as_ref()
            .and_then(|map| map.get(&file.new_blob))
            .copied();
        let parent_content = if file.old_blob.is_none() {
            Some(Vec::new())
        } else {
            contents
                .as_ref()
                .and_then(|map| file.old_blob.as_ref().and_then(|id| map.get(id)).cloned())
        };
        let committed_content = contents
            .as_ref()
            .and_then(|map| map.get(&file.new_blob).cloned());

        let Some(parent_content) = parent_content else {
            output_files.push(unknown_file(file.path, "missing_blob", 0));
            continue;
        };
        let Some(committed_content) = committed_content else {
            let evidence = if new_size.is_some_and(|size| size > MAX_FILE)
                || old_size.is_some_and(|size| size > MAX_FILE)
            {
                "skipped_large"
            } else {
                "missing_blob"
            };
            if evidence == "missing_blob" {
                output_files.push(unknown_file(file.path, evidence, 0));
            } else {
                output_files.push(skipped_file(file.path, &parent_content, &[], evidence));
            }
            continue;
        };

        let added_count = added_nonblank(&parent_content, &committed_content);
        if crate::is_excluded(&file.path) {
            output_files.push(skipped_file(
                file.path,
                &parent_content,
                &committed_content,
                "skipped_excluded",
            ));
            continue;
        }
        if parent_content[..parent_content.len().min(8000)].contains(&0)
            || committed_content[..committed_content.len().min(8000)].contains(&0)
        {
            output_files.push(skipped_file(
                file.path,
                &parent_content,
                &committed_content,
                "skipped_binary",
            ));
            continue;
        }
        if old_size.is_some_and(|size| size > MAX_FILE)
            || new_size.is_some_and(|size| size > MAX_FILE)
        {
            output_files.push(skipped_file(
                file.path,
                &parent_content,
                &committed_content,
                "skipped_large",
            ));
            continue;
        }

        let selected_events =
            events_for_path(&event_records, &file.path, parent_time, &parent_content);
        let (mut steps, mut evidence) =
            steps_for_events(&store, &selected_events, &committed_content, commit_time);
        if evidence == "missing_blob" {
            output_files.push(unknown_file(file.path, evidence, added_count));
            continue;
        }

        if (reconstruct_all || reconstruct_paths.contains(file.path.as_str()))
            && steps.last().map(|step| step.after.as_slice()) != Some(committed_content.as_slice())
        {
            steps.push(Step {
                kind: StepKind::Ai { degraded: true },
                after: committed_content.clone(),
            });
            evidence = "reconstructed";
        }
        let labels = derive::derive_lines(
            &Evidence {
                parent: parent_content,
                steps,
            },
            &committed_content,
        );
        let counts = derive::count(&labels);
        output_files.push(FileOutput {
            path: file.path,
            e: counts.e,
            ai_strong: counts.s,
            ai_probable: counts.p,
            mixed: counts.m,
            unknown: counts.u,
            excluded: 0,
            evidence,
        });
    }

    output_files.sort_by(|left, right| left.path.cmp(&right.path));
    let mut totals = Totals::default();
    let mut covered = 0usize;
    for file in &output_files {
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
    totals.ai_deterministic_pct = percentage_tenths(totals.ai_direct + totals.ai_strong, totals.e);
    totals.ai_linked_pct = percentage_tenths(
        totals.ai_direct + totals.ai_strong + totals.ai_probable + totals.mixed,
        totals.e,
    );
    let coverage = coverage_band(covered, totals.e);
    let bytes = serde_json::to_vec(&CommitOutput {
        schema: "prov.commit/0.1",
        repo_id,
        commit: commit.clone(),
        parent,
        totals,
        coverage,
        files: output_files,
    })
    .map_err(|error| error.to_string())?;
    crate::ensure_private_dir(output_path.parent().ok_or("invalid commit path")?)
        .map_err(|error| error.to_string())?;
    crate::atomic_bytes(&output_path, &bytes, false).map_err(|error| error.to_string())?;
    Ok(output_path)
}

pub fn derive_range(repo_path: &Path, range: &str) -> Result<Vec<PathBuf>, String> {
    if !range.contains("..") {
        return Err("range must use A..B syntax".into());
    }
    let (repo, _, _) = repo_context(repo_path)?;
    let commits = git_checked(
        &repo.root,
        &["rev-list", "--reverse", "--end-of-options", range],
    )?;
    let mut paths = Vec::new();
    for commit in commits.lines().filter(|line| !line.is_empty()) {
        paths.push(derive_commit(&repo.root, commit)?);
    }
    if paths.is_empty() {
        return Err("range contains no commits".into());
    }
    Ok(paths)
}

pub fn show_commit(repo_path: &Path, revision: &str) -> Result<Vec<u8>, String> {
    let (repo, _, store) = repo_context(repo_path)?;
    let commit = resolve_commit(&repo.root, revision)?;
    fs::read(store.join("commits").join(format!("{commit}.json")))
        .map_err(|_| format!("commit {commit} has not been derived"))
}

pub fn summary(bytes: &[u8]) -> Result<String, String> {
    let value: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|error| error.to_string())?;
    if let Some(skipped) = value.get("skipped").and_then(serde_json::Value::as_str) {
        return Ok(format!("{} skipped", skipped));
    }
    let totals = value.get("totals").ok_or("missing totals")?;
    Ok(format!(
        "{}: {} added lines ({} strong, {} probable, {} mixed, {} unknown), coverage {}",
        value["commit"].as_str().unwrap_or("unknown"),
        totals["e"].as_u64().unwrap_or(0),
        totals["ai_strong"].as_u64().unwrap_or(0),
        totals["ai_probable"].as_u64().unwrap_or(0),
        totals["mixed"].as_u64().unwrap_or(0),
        totals["unknown"].as_u64().unwrap_or(0),
        value["coverage"].as_str().unwrap_or("none"),
    ))
}

fn repo_context(path: &Path) -> Result<(crate::RepoInfo, String, PathBuf), String> {
    let home = crate::store_home().ok_or("could not determine store directory")?;
    let repo = crate::repo_info(path, &home).ok_or("not a supported Git repository")?;
    let repo_id = repo
        .repo_id
        .clone()
        .ok_or("repository identity unavailable")?;
    let store = home.join("repos").join(&repo_id);
    Ok((repo, repo_id, store))
}

fn resolve_commit(root: &Path, revision: &str) -> Result<String, String> {
    let revision = format!("{revision}^{{commit}}");
    let resolved = git_checked(
        root,
        &["rev-parse", "--verify", "--end-of-options", &revision],
    )?;
    Ok(resolved.trim().to_owned())
}

fn committer_time_ns(root: &Path, commit: &str) -> Result<u128, String> {
    let seconds = git_checked(root, &["show", "-s", "--format=%ct", commit])?
        .trim()
        .parse::<u128>()
        .map_err(|error| error.to_string())?;
    Ok(seconds.saturating_mul(NANOSECOND))
}

fn commit_files(
    root: &Path,
    parent: Option<&str>,
    commit: &str,
) -> Result<Vec<CommitFile>, String> {
    let output = if let Some(parent) = parent {
        git_output(
            root,
            &[
                "diff-tree",
                "-r",
                "--no-renames",
                "-z",
                "--raw",
                "--no-commit-id",
                parent,
                commit,
            ],
        )?
    } else {
        git_output(
            root,
            &[
                "diff-tree",
                "--root",
                "-r",
                "--no-renames",
                "-z",
                "--raw",
                "--no-commit-id",
                commit,
            ],
        )?
    };
    if !output.status.success() {
        return Err("git diff-tree failed".into());
    }
    let mut records = output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|item| !item.is_empty());
    let mut files = Vec::new();
    while let Some(header) = records.next() {
        let Some(path) = records.next() else {
            break;
        };
        let fields: Vec<_> = header.split(|byte| *byte == b' ').collect();
        if fields.len() < 5 || fields[4].first() == Some(&b'D') {
            continue;
        }
        let old_id = String::from_utf8_lossy(fields[2]).into_owned();
        let new_id = String::from_utf8_lossy(fields[3]).into_owned();
        let old_blob = (!old_id.bytes().all(|byte| byte == b'0')).then_some(old_id);
        if new_id.bytes().all(|byte| byte == b'0') {
            continue;
        }
        files.push(CommitFile {
            path: String::from_utf8_lossy(path).replace('\\', "/"),
            old_blob,
            new_blob: new_id,
        });
    }
    Ok(files)
}

fn read_events(events_dir: &Path) -> Vec<EventRecord> {
    let Ok(entries) = fs::read_dir(events_dir) else {
        return Vec::new();
    };
    let mut paths: Vec<_> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "jsonl")
        })
        .collect();
    paths.sort();
    let mut events = Vec::new();
    let mut order = 0;
    for path in paths {
        let Ok(bytes) = fs::read(path) else {
            continue;
        };
        for line in bytes
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
        {
            let Ok(mut event) = serde_json::from_slice::<EventRecord>(line) else {
                continue;
            };
            event.order = order;
            order += 1;
            events.push(event);
        }
    }
    events.sort_by_key(|event| (event.timestamp, event.order));
    events
}

fn events_for_path<'a>(
    events: &'a [EventRecord],
    path: &str,
    parent_time: u128,
    parent_content: &[u8],
) -> Vec<&'a EventRecord> {
    let parent_sha = crate::hash(parent_content);
    let mut matched: Vec<_> = events
        .iter()
        .filter(|event| {
            event.path.as_deref() == Some(path)
                && matches!(
                    event.event.as_str(),
                    "ai_change" | "external_change" | "deletion"
                )
                && event.timestamp > parent_time
        })
        .collect();
    if let Some(latest_parent) = matched
        .iter()
        .rposition(|event| event.after_sha.as_deref() == Some(parent_sha.as_str()))
    {
        matched.drain(..=latest_parent);
    }
    matched
}

fn steps_for_events(
    store: &Path,
    events: &[&EventRecord],
    committed: &[u8],
    commit_time: u128,
) -> (Vec<Step>, &'static str) {
    if events.is_empty() {
        return (Vec::new(), "none");
    }
    let committed_sha = crate::hash(committed);
    let exact_end = events
        .iter()
        .rposition(|event| event.after_sha.as_deref() == Some(committed_sha.as_str()));
    let (selected, evidence): (Vec<_>, &'static str) = if let Some(end) = exact_end {
        (events[..=end].to_vec(), "exact")
    } else {
        let end_time = commit_time.saturating_add(NANOSECOND - 1);
        let filtered: Vec<_> = events
            .iter()
            .copied()
            .filter(|event| event.timestamp <= end_time)
            .collect();
        if filtered.is_empty() {
            return (Vec::new(), "none");
        }
        (filtered, "time_cut")
    };

    let mut steps = Vec::with_capacity(selected.len());
    for event in selected {
        let after = if event.event == "deletion" || event.after_sha.is_none() {
            Vec::new()
        } else {
            let sha = event.after_sha.as_deref().unwrap_or_default();
            let Some(bytes) = crate::read_blob(&store.join("blobs").join(sha), sha) else {
                return (Vec::new(), "missing_blob");
            };
            bytes
        };
        let kind = match event.event.as_str() {
            "ai_change" => StepKind::Ai {
                degraded: event.degraded.unwrap_or(false) || event.overlap.unwrap_or(false),
            },
            "external_change"
                if event.degraded.unwrap_or(false)
                    || event.reason.as_deref() == Some("before_unavailable") =>
            {
                StepKind::Unknown
            }
            "external_change" => StepKind::External,
            "deletion" => StepKind::External,
            _ => continue,
        };
        steps.push(Step { kind, after });
    }
    (steps, evidence)
}

fn added_nonblank(parent: &[u8], committed: &[u8]) -> usize {
    derive::count(&derive::derive_lines(
        &Evidence {
            parent: parent.to_vec(),
            steps: Vec::new(),
        },
        committed,
    ))
    .e
}

fn skipped_file(
    path: String,
    parent: &[u8],
    committed: &[u8],
    evidence: &'static str,
) -> FileOutput {
    FileOutput {
        path,
        e: 0,
        ai_strong: 0,
        ai_probable: 0,
        mixed: 0,
        unknown: 0,
        excluded: added_nonblank(parent, committed),
        evidence,
    }
}

fn unknown_file(path: String, evidence: &'static str, added: usize) -> FileOutput {
    FileOutput {
        path,
        e: added,
        ai_strong: 0,
        ai_probable: 0,
        mixed: 0,
        unknown: added,
        excluded: 0,
        evidence,
    }
}

fn percentage_tenths(numerator: usize, denominator: usize) -> Option<f64> {
    if denominator == 0 {
        None
    } else {
        let tenths =
            ((numerator as u128 * 1000 + denominator as u128 / 2) / denominator as u128) as u64;
        Some(tenths as f64 / 10.0)
    }
}

fn coverage_band(covered: usize, total: usize) -> &'static str {
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

fn git_output(root: &Path, args: &[&str]) -> Result<Output, String> {
    Command::new("git")
        .args(args)
        .current_dir(root)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .output()
        .map_err(|error| error.to_string())
}

fn git_checked(root: &Path, args: &[&str]) -> Result<String, String> {
    let output = git_output(root, args)?;
    if !output.status.success() {
        return Err(format!("git {} failed", args.join(" ")));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}
