use crate::derive::{self, Evidence, Step, StepKind};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant, SystemTime};

const MAX_FILE: usize = 1024 * 1024;
const MAX_PATCH_DIFF_SIZE: usize = 8 * 1024 * 1024;
const NANOSECOND: u128 = 1_000_000_000;
const MAX_PENDING_PER_RUN: usize = 50;
const PENDING_BUDGET: Duration = Duration::from_secs(20);
const ATTEMPT_MARKER_AGE: Duration = Duration::from_secs(60);
const DERIVE_LOCK_AGE: Duration = Duration::from_secs(120);

#[derive(Serialize)]
struct CommitOutput {
    schema: &'static str,
    repo_id: String,
    commit: String,
    parent: Option<String>,
    totals: Totals,
    coverage: &'static str,
    files: Vec<FileOutput>,
    patch_id: Option<String>,
    rewrite: RewriteOutput,
}

#[derive(Serialize)]
struct MergeOutput {
    schema: &'static str,
    commit: String,
    skipped: &'static str,
    patch_id: Option<String>,
    rewrite: RewriteOutput,
}

#[derive(Serialize)]
struct LinesOutput {
    schema: &'static str,
    repo_id: String,
    commit: String,
    totals: LineTotals,
    files: Vec<LineFileOutput>,
}

#[derive(Serialize)]
struct LineTotals {
    e: usize,
    ai_strong: usize,
    ai_probable: usize,
    mixed: usize,
    unknown: usize,
}

#[derive(Serialize)]
struct LineFileOutput {
    path: String,
    lines: Vec<LineOutput>,
}

#[derive(Serialize)]
struct LineOutput {
    line: usize,
    hash: String,
    label: char,
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
    unreadable_files: usize,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    carried: Option<usize>,
}

#[derive(Serialize)]
struct RewriteOutput {
    method: &'static str,
    sources: Vec<String>,
    carried_lines: usize,
}

#[derive(Clone)]
struct CommitFile {
    path: String,
    old_blob: Option<String>,
    new_blob: String,
    added_lines: usize,
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
    let events = read_events(&store.join("events"));
    let event_index = index_events(&events);
    derive_commit_with_events(&repo, &repo_id, &store, revision, &events, &event_index)
}

pub fn derive_pending(repo_path: &Path) -> Result<Vec<PathBuf>, String> {
    let (repo, repo_id, store) = repo_context(repo_path)?;
    crate::ensure_private_dir(&store).map_err(|error| error.to_string())?;
    let Some(_lock) = acquire_derive_lock(&store.join("derive.lock"))? else {
        return Ok(Vec::new());
    };

    let started = Instant::now();
    let events = read_events(&store.join("events"));
    let event_index = index_events(&events);
    let commits_dir = store.join("commits");
    crate::ensure_private_dir(&commits_dir).map_err(|error| error.to_string())?;
    let pending = pending_candidates(&commits_dir, &events);
    let mut attempted = 0;
    let mut derived = Vec::new();
    for candidate in pending {
        if attempted >= MAX_PENDING_PER_RUN || started.elapsed() >= PENDING_BUDGET {
            break;
        }
        let commit = candidate.commit;
        if !commit_exists(&repo.root, &commit) {
            let gone = commits_dir.join(format!("{commit}.gone"));
            let _ = OpenOptions::new().write(true).create_new(true).open(gone);
            continue;
        }
        let marker = commits_dir.join(format!("{commit}.attempt"));
        if !create_attempt_marker(&marker)? {
            continue;
        }
        attempted += 1;
        match derive_resolved_commit(&repo, &repo_id, &store, commit, &events, &event_index) {
            Ok(path) => {
                let _ = fs::remove_file(marker);
                derived.push(path);
            }
            Err(_) => continue,
        }
    }
    Ok(derived)
}

pub fn pending_status(repo_path: &Path) -> Result<(usize, Option<u128>), String> {
    let (_repo, _, store) = repo_context(repo_path)?;
    let events = read_events(&store.join("events"));
    let commits_dir = store.join("commits");
    let pending = pending_candidates(&commits_dir, &events);
    let oldest = pending.iter().map(|item| item.event_timestamp).min();
    Ok((pending.len(), oldest))
}

pub fn pending_status_at(
    repo_path: &Path,
    repo_store: &Path,
) -> Result<(usize, Option<u128>), String> {
    crate::find_git_dirs(repo_path).ok_or("not a supported Git repository")?;
    let events = read_events(&repo_store.join("events"));
    let commits_dir = repo_store.join("commits");
    let pending = pending_candidates(&commits_dir, &events);
    let oldest = pending.iter().map(|item| item.event_timestamp).min();
    Ok((pending.len(), oldest))
}

struct PendingCommit {
    commit: String,
    event_timestamp: u128,
}

fn pending_candidates(commits_dir: &Path, events: &[EventRecord]) -> Vec<PendingCommit> {
    let mut references = BTreeMap::new();
    for event in events.iter().filter(|event| event.event == "head_move") {
        let Some(revision) = event.new_head.as_deref() else {
            continue;
        };
        references
            .entry(revision.to_owned())
            .and_modify(|timestamp: &mut u128| *timestamp = (*timestamp).min(event.timestamp))
            .or_insert(event.timestamp);
    }

    let mut pending = Vec::new();
    for (revision, event_timestamp) in references {
        if !matches!(revision.len(), 40 | 64)
            || !revision.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            continue;
        }
        if commits_dir.join(format!("{revision}.json")).exists() {
            continue;
        }
        if commits_dir.join(format!("{revision}.gone")).exists() {
            continue;
        }
        pending.push(PendingCommit {
            commit: revision,
            event_timestamp,
        });
    }
    pending.sort_by(|left, right| {
        (left.event_timestamp, &left.commit).cmp(&(right.event_timestamp, &right.commit))
    });
    pending
}

fn commit_exists(root: &Path, commit: &str) -> bool {
    let revision = format!("{commit}^{{commit}}");
    git_output(root, &["cat-file", "-e", &revision]).is_ok_and(|output| output.status.success())
}

struct DeriveLock(PathBuf);

impl Drop for DeriveLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn acquire_derive_lock(path: &Path) -> Result<Option<DeriveLock>, String> {
    for _ in 0..3 {
        match OpenOptions::new().write(true).create_new(true).open(path) {
            Ok(mut file) => {
                if writeln!(file, "{}", std::process::id()).is_err() {
                    let _ = fs::remove_file(path);
                    return Err("could not write derive lock".into());
                }
                return Ok(Some(DeriveLock(path.to_path_buf())));
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                if is_recent(path, DERIVE_LOCK_AGE) {
                    return Ok(None);
                }
                match fs::remove_file(path) {
                    Ok(()) => continue,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(error) => return Err(error.to_string()),
                }
            }
            Err(error) => return Err(error.to_string()),
        }
    }
    Ok(None)
}

fn create_attempt_marker(path: &Path) -> Result<bool, String> {
    if path.exists() {
        if is_recent(path, ATTEMPT_MARKER_AGE) {
            return Ok(false);
        }
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.to_string()),
        }
    }
    match OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(mut file) => {
            if writeln!(file, "{}", std::process::id()).is_err() {
                let _ = fs::remove_file(path);
                return Err("could not write derive attempt marker".into());
            }
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(error) => Err(error.to_string()),
    }
}

fn is_recent(path: &Path, max_age: Duration) -> bool {
    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .is_some_and(|modified| {
            SystemTime::now()
                .duration_since(modified)
                .unwrap_or_default()
                < max_age
        })
}

fn derive_commit_with_events(
    repo: &crate::RepoInfo,
    repo_id: &str,
    store: &Path,
    revision: &str,
    event_records: &[EventRecord],
    event_index: &HashMap<String, Vec<usize>>,
) -> Result<PathBuf, String> {
    let commit = resolve_commit(&repo.root, revision)?;
    derive_resolved_commit(repo, repo_id, store, commit, event_records, event_index)
}

fn derive_resolved_commit(
    repo: &crate::RepoInfo,
    repo_id: &str,
    store: &Path,
    commit: String,
    event_records: &[EventRecord],
    event_index: &HashMap<String, Vec<usize>>,
) -> Result<PathBuf, String> {
    let parent_line = git_checked(&repo.root, &["rev-list", "--parents", "-n", "1", &commit])?;
    let commits: Vec<_> = parent_line.split_whitespace().collect();
    if commits.first() != Some(&commit.as_str()) {
        return Err("could not read commit ancestry".into());
    }
    let parents = &commits[1..];
    let output_path = store.join("commits").join(format!("{commit}.json"));

    if parents.len() > 1 {
        let lines_path = store.join("commits").join(format!("{commit}.lines.json"));
        let lines_bytes = serde_json::to_vec(&LinesOutput {
            schema: "prov.lines/0.1",
            repo_id: repo_id.to_owned(),
            commit: commit.clone(),
            totals: LineTotals {
                e: 0,
                ai_strong: 0,
                ai_probable: 0,
                mixed: 0,
                unknown: 0,
            },
            files: Vec::new(),
        })
        .map_err(|error| error.to_string())?;
        crate::ensure_private_dir(lines_path.parent().ok_or("invalid lines path")?)
            .map_err(|error| error.to_string())?;
        crate::atomic_bytes(&lines_path, &lines_bytes, false).map_err(|error| error.to_string())?;
        let bytes = serde_json::to_vec(&MergeOutput {
            schema: "prov.commit/0.1",
            commit,
            skipped: "merge_commit",
            patch_id: None,
            rewrite: RewriteOutput {
                method: "none",
                sources: Vec::new(),
                carried_lines: 0,
            },
        })
        .map_err(|error| error.to_string())?;
        crate::ensure_private_dir(output_path.parent().ok_or("invalid commit path")?)
            .map_err(|error| error.to_string())?;
        crate::atomic_bytes(&output_path, &bytes, false).map_err(|error| error.to_string())?;
        return Ok(output_path);
    }

    let parent = parents.first().map(|value| (*value).to_owned());
    let (files, has_changes) = commit_files(&repo.root, parent.as_deref(), &commit)?;
    if !has_changes {
        return write_empty_commit(store, repo_id, commit, parent);
    }
    let parent_time = parent
        .as_deref()
        .map(|value| committer_time_ns(&repo.root, value))
        .transpose()?
        .unwrap_or(0);
    let commit_time = committer_time_ns(&repo.root, &commit)?;
    let mut blob_ids = BTreeSet::new();
    for file in &files {
        blob_ids.insert(file.new_blob.clone());
        if let Some(old) = &file.old_blob {
            blob_ids.insert(old.clone());
        }
    }
    let ids: Vec<_> = blob_ids.into_iter().collect();
    let sizes = crate::check_blob_sizes(&repo.root, &ids);
    let available_ids = sizes
        .as_ref()
        .map(|sizes| readable_blob_ids(sizes, &ids))
        .unwrap_or_default();
    let contents = crate::cat_blobs(&repo.root, &available_ids);
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
    let mut line_files = BTreeMap::new();
    let mut unreadable_files = 0;
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
        if old_size.is_some_and(|size| size > MAX_FILE)
            || new_size.is_some_and(|size| size > MAX_FILE)
        {
            output_files.push(skipped_file_added(
                file.path,
                file.added_lines,
                "skipped_large",
            ));
            continue;
        }
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
            unreadable_files += 1;
            output_files.push(unknown_file(file.path, "missing_blob", 0));
            continue;
        };
        let Some(committed_content) = committed_content else {
            unreadable_files += 1;
            output_files.push(unknown_file(file.path, "missing_blob", 0));
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

        let normalized_parent = normalize_eol(&parent_content);
        let normalized_committed = normalize_eol(&committed_content);
        let selected_events = events_for_path(
            store,
            event_records,
            event_index,
            &file.path,
            parent_time,
            &normalized_parent,
        );
        let (mut steps, mut evidence) =
            steps_for_events(store, &selected_events, &normalized_committed, commit_time);
        if evidence == "missing_blob" {
            unreadable_files += 1;
            output_files.push(unknown_file(file.path, evidence, added_count));
            continue;
        }

        if (reconstruct_all || reconstruct_paths.contains(file.path.as_str()))
            && steps.last().map(|step| step.after.as_slice())
                != Some(normalized_committed.as_slice())
        {
            steps.push(Step {
                kind: StepKind::Ai { degraded: true },
                after: normalized_committed.clone(),
            });
            evidence = "reconstructed";
        }
        let labels = derive::derive_lines(
            &Evidence {
                parent: normalized_parent,
                steps,
            },
            &normalized_committed,
        );
        let counts = derive::count(&labels);
        let committed_lines = split_normalized_lines(&normalized_committed);
        let lines = labels
            .iter()
            .filter(|line| line.added && !line.blank)
            .filter_map(|line| {
                let text = committed_lines.get(line.line.checked_sub(1)?)?;
                let digest = crate::hash(text);
                Some(LineOutput {
                    line: line.line,
                    hash: digest[..16].to_owned(),
                    label: match line.label {
                        derive::Label::AiStrong => 's',
                        derive::Label::AiProbable => 'p',
                        derive::Label::Mixed => 'm',
                        derive::Label::Unknown => 'u',
                    },
                })
            })
            .collect();
        line_files.insert(file.path.clone(), lines);
        output_files.push(FileOutput {
            path: file.path,
            e: counts.e,
            ai_strong: counts.s,
            ai_probable: counts.p,
            mixed: counts.m,
            unknown: counts.u,
            excluded: 0,
            evidence,
            carried: None,
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
    totals.unreadable_files = unreadable_files;
    totals.ai_deterministic_pct = percentage_tenths(totals.ai_direct + totals.ai_strong, totals.e);
    totals.ai_linked_pct = percentage_tenths(
        totals.ai_direct + totals.ai_strong + totals.ai_probable + totals.mixed,
        totals.e,
    );
    let coverage = coverage_band(covered, totals.e);
    let patch_id = if has_changes {
        parent
            .as_deref()
            .and_then(|parent| stable_patch_id(&repo.root, parent, &commit))
    } else {
        None
    };
    let sidecar_files = output_files
        .iter()
        .map(|file| LineFileOutput {
            path: file.path.clone(),
            lines: line_files.remove(&file.path).unwrap_or_default(),
        })
        .collect();
    let lines_bytes = serde_json::to_vec(&LinesOutput {
        schema: "prov.lines/0.1",
        repo_id: repo_id.to_owned(),
        commit: commit.clone(),
        totals: LineTotals {
            e: totals.e,
            ai_strong: totals.ai_strong,
            ai_probable: totals.ai_probable,
            mixed: totals.mixed,
            unknown: totals.unknown,
        },
        files: sidecar_files,
    })
    .map_err(|error| error.to_string())?;
    let lines_path = store.join("commits").join(format!("{commit}.lines.json"));
    crate::ensure_private_dir(lines_path.parent().ok_or("invalid lines path")?)
        .map_err(|error| error.to_string())?;
    crate::atomic_bytes(&lines_path, &lines_bytes, false).map_err(|error| error.to_string())?;
    let bytes = serde_json::to_vec(&CommitOutput {
        schema: "prov.commit/0.1",
        repo_id: repo_id.to_owned(),
        commit: commit.clone(),
        parent,
        totals,
        coverage,
        files: output_files,
        patch_id,
        rewrite: RewriteOutput {
            method: "none",
            sources: Vec::new(),
            carried_lines: 0,
        },
    })
    .map_err(|error| error.to_string())?;
    crate::ensure_private_dir(output_path.parent().ok_or("invalid commit path")?)
        .map_err(|error| error.to_string())?;
    crate::atomic_bytes(&output_path, &bytes, false).map_err(|error| error.to_string())?;
    let _ = crate::rewrite::apply(&repo.root, store, &commit);
    Ok(output_path)
}

fn write_empty_commit(
    store: &Path,
    repo_id: &str,
    commit: String,
    parent: Option<String>,
) -> Result<PathBuf, String> {
    let commits_dir = store.join("commits");
    let output_path = commits_dir.join(format!("{commit}.json"));
    let lines_path = commits_dir.join(format!("{commit}.lines.json"));
    let lines_bytes = serde_json::to_vec(&LinesOutput {
        schema: "prov.lines/0.1",
        repo_id: repo_id.to_owned(),
        commit: commit.clone(),
        totals: LineTotals {
            e: 0,
            ai_strong: 0,
            ai_probable: 0,
            mixed: 0,
            unknown: 0,
        },
        files: Vec::new(),
    })
    .map_err(|error| error.to_string())?;
    let bytes = serde_json::to_vec(&CommitOutput {
        schema: "prov.commit/0.1",
        repo_id: repo_id.to_owned(),
        commit,
        parent,
        totals: Totals::default(),
        coverage: "none",
        files: Vec::new(),
        patch_id: None,
        rewrite: RewriteOutput {
            method: "none",
            sources: Vec::new(),
            carried_lines: 0,
        },
    })
    .map_err(|error| error.to_string())?;
    crate::ensure_private_dir(&commits_dir).map_err(|error| error.to_string())?;
    crate::atomic_bytes(&lines_path, &lines_bytes, false).map_err(|error| error.to_string())?;
    crate::atomic_bytes(&output_path, &bytes, false).map_err(|error| error.to_string())?;
    Ok(output_path)
}

fn split_normalized_lines(bytes: &[u8]) -> Vec<&[u8]> {
    let mut lines: Vec<_> = bytes.split(|byte| *byte == b'\n').collect();
    if bytes.last() == Some(&b'\n') {
        lines.pop();
    }
    lines
}

fn stable_patch_id(root: &Path, parent: &str, commit: &str) -> Option<String> {
    let diff = git_output(root, &["diff-tree", "-p", "--no-renames", parent, commit]).ok()?;
    if !diff.status.success() || !patch_diff_size_allowed(diff.stdout.len()) {
        return None;
    }
    let mut child = Command::new("git")
        .args(["patch-id", "--stable"])
        .current_dir(root)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    child.stdin.take()?.write_all(&diff.stdout).ok()?;
    let output = child.wait_with_output().ok()?;
    if !output.status.success() {
        return None;
    }
    let patch_id = String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .next()?
        .to_owned();
    (patch_id.len() == 40 && patch_id.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then_some(patch_id)
}

fn patch_diff_size_allowed(size: usize) -> bool {
    size <= MAX_PATCH_DIFF_SIZE
}

pub fn derive_range(repo_path: &Path, range: &str) -> Result<Vec<PathBuf>, String> {
    if !range.contains("..") {
        return Err("range must use A..B syntax".into());
    }
    let (repo, repo_id, store) = repo_context(repo_path)?;
    let events = read_events(&store.join("events"));
    let event_index = index_events(&events);
    let commits = git_checked(
        &repo.root,
        &["rev-list", "--reverse", "--end-of-options", range],
    )?;
    let mut paths = Vec::new();
    for commit in commits.lines().filter(|line| !line.is_empty()) {
        paths.push(derive_commit_with_events(
            &repo,
            &repo_id,
            &store,
            commit,
            &events,
            &event_index,
        )?);
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
) -> Result<(Vec<CommitFile>, bool), String> {
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
    let has_changes = !output.stdout.is_empty();
    if !has_changes {
        return Ok((Vec::new(), false));
    }
    let additions = numstat_additions(root, parent, commit)?;
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
        let old_mode = fields[0].strip_prefix(b":").unwrap_or(fields[0]);
        if matches!(old_mode, b"120000" | b"160000") || matches!(fields[1], b"120000" | b"160000") {
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
            added_lines: additions
                .get(&String::from_utf8_lossy(path).replace('\\', "/"))
                .copied()
                .unwrap_or_default(),
        });
    }
    Ok((files, has_changes))
}

fn numstat_additions(
    root: &Path,
    parent: Option<&str>,
    commit: &str,
) -> Result<std::collections::HashMap<String, usize>, String> {
    let args = if let Some(parent) = parent {
        vec![
            "diff-tree",
            "-r",
            "--no-renames",
            "-z",
            "--numstat",
            "--no-commit-id",
            parent,
            commit,
        ]
    } else {
        vec![
            "diff-tree",
            "--root",
            "-r",
            "--no-renames",
            "-z",
            "--numstat",
            "--no-commit-id",
            commit,
        ]
    };
    let output = git_output(root, &args)?;
    if !output.status.success() {
        return Ok(std::collections::HashMap::new());
    }
    let mut additions = std::collections::HashMap::new();
    for record in output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|item| !item.is_empty())
    {
        let fields: Vec<_> = record.splitn(3, |byte| *byte == b'\t').collect();
        if fields.len() != 3 || fields[0] == b"-" {
            continue;
        }
        let Ok(count) = std::str::from_utf8(fields[0]).unwrap_or_default().parse() else {
            continue;
        };
        additions.insert(String::from_utf8_lossy(fields[2]).replace('\\', "/"), count);
    }
    Ok(additions)
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

fn index_events(events: &[EventRecord]) -> HashMap<String, Vec<usize>> {
    let mut index: HashMap<String, Vec<usize>> = HashMap::new();
    for (event_index, event) in events.iter().enumerate() {
        if matches!(
            event.event.as_str(),
            "ai_change" | "external_change" | "deletion"
        ) {
            if let Some(path) = &event.path {
                index.entry(path.clone()).or_default().push(event_index);
            }
        }
    }
    index
}

fn events_for_path<'a>(
    store: &Path,
    events: &'a [EventRecord],
    event_index: &HashMap<String, Vec<usize>>,
    path: &str,
    parent_time: u128,
    parent_content: &[u8],
) -> Vec<&'a EventRecord> {
    let mut matched: Vec<_> = event_index
        .get(path)
        .into_iter()
        .flatten()
        .map(|&index| &events[index])
        .filter(|event| event.timestamp > parent_time)
        .collect();
    let start = matched.len().saturating_sub(64);
    for index in (start..matched.len()).rev() {
        let Some(sha) = matched[index].after_sha.as_deref() else {
            continue;
        };
        if !is_sha256(sha) {
            continue;
        }
        let Some(bytes) = crate::read_blob(&store.join("blobs").join(sha), sha) else {
            continue;
        };
        if normalize_eol(&bytes) == parent_content {
            matched.drain(..=index);
            break;
        }
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
    let exact_end = events
        .iter()
        .enumerate()
        .filter_map(|(index, event)| {
            let sha = event.after_sha.as_deref()?;
            if !is_sha256(sha) {
                return None;
            }
            let bytes = crate::read_blob(&store.join("blobs").join(sha), sha)?;
            (normalize_eol(&bytes) == committed).then_some(index)
        })
        .next_back();
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
            if !is_sha256(sha) {
                return (Vec::new(), "missing_blob");
            }
            let Some(bytes) = crate::read_blob(&store.join("blobs").join(sha), sha) else {
                return (Vec::new(), "missing_blob");
            };
            normalize_eol(&bytes)
        };
        let kind = match event.event.as_str() {
            "ai_change" => StepKind::Ai {
                degraded: evidence == "time_cut"
                    || event.degraded.unwrap_or(false)
                    || event.overlap.unwrap_or(false),
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

fn normalize_eol(bytes: &[u8]) -> Vec<u8> {
    let mut normalized = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\r' && bytes.get(index + 1) == Some(&b'\n') {
            normalized.push(b'\n');
            index += 2;
        } else {
            normalized.push(bytes[index]);
            index += 1;
        }
    }
    normalized
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn readable_blob_ids(sizes: &BTreeMap<String, usize>, ids: &[String]) -> Vec<String> {
    ids.iter()
        .filter(|id| sizes.get(*id).is_some_and(|size| *size <= MAX_FILE))
        .cloned()
        .collect()
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
        carried: None,
    }
}

fn skipped_file_added(path: String, added_lines: usize, evidence: &'static str) -> FileOutput {
    FileOutput {
        path,
        e: 0,
        ai_strong: 0,
        ai_probable: 0,
        mixed: 0,
        unknown: 0,
        excluded: added_lines,
        evidence,
        carried: None,
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
        carried: None,
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

#[cfg(test)]
mod tests {
    use super::{patch_diff_size_allowed, readable_blob_ids, MAX_FILE, MAX_PATCH_DIFF_SIZE};
    use std::collections::BTreeMap;

    #[test]
    fn readable_blob_ids_enforces_the_one_mib_limit_and_requires_known_sizes() {
        let sizes = BTreeMap::from([
            ("at-limit".to_owned(), MAX_FILE),
            ("over-limit".to_owned(), MAX_FILE + 1),
        ]);
        let ids = vec![
            "at-limit".to_owned(),
            "over-limit".to_owned(),
            "missing".to_owned(),
        ];
        assert_eq!(readable_blob_ids(&sizes, &ids), vec!["at-limit"]);
    }

    #[test]
    fn patch_id_diff_size_limit_includes_eight_mib_and_rejects_larger() {
        assert!(patch_diff_size_allowed(MAX_PATCH_DIFF_SIZE));
        assert!(!patch_diff_size_allowed(MAX_PATCH_DIFF_SIZE + 1));
    }
}
