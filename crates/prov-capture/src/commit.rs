use crate::derive::{self, Evidence, Step, StepKind};
use serde::{Deserialize, Serialize};
use similar::{capture_diff_slices, Algorithm, DiffTag};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const MAX_FILE: usize = 1024 * 1024;
const MAX_PATCH_DIFF_SIZE: usize = 8 * 1024 * 1024;
const NANOSECOND: u128 = 1_000_000_000;
const MAX_PENDING_PER_RUN: usize = 50;
const PENDING_BUDGET: Duration = Duration::from_secs(20);
const ATTEMPT_MARKER_AGE: Duration = Duration::from_secs(60);
const DERIVE_LOCK_AGE: Duration = Duration::from_secs(120);
const MIN_PARENT_EDIT_SIMILARITY: f64 = 0.5;
const MAX_PARENT_MATCH_CELLS: usize = 1_000_000;
const MIXED_SCORE_MILLI: u16 = 840;
const UNOBSERVED_COVERAGE_MEDIUM_PERCENT: usize = 5;
const UNOBSERVED_COVERAGE_LOW_PERCENT: usize = 20;
const UNOBSERVED_COVERAGE_BASIS: [&str; 2] = ["X_UNOBSERVED_EDIT", "X_UNOBSERVED_EDIT_IN_FILE"];

#[derive(Clone, Default)]
struct StoredParentLine {
    category: String,
    score: Option<f64>,
    supporting_basis: Vec<String>,
    source: Option<AttributionSource>,
    operation: Option<String>,
    exclusion_classifier: Option<String>,
    exclusion_reason: Option<String>,
}

impl StoredParentLine {
    fn is_ai(&self) -> bool {
        matches!(
            self.category.as_str(),
            "ai_direct" | "ai_strong" | "ai_probable" | "mixed"
        )
    }
}

#[derive(Serialize)]
struct CommitOutput {
    schema: &'static str,
    repo_id: String,
    commit: String,
    parent: Option<String>,
    totals: Totals,
    coverage: &'static str,
    coverage_unobserved_lines: usize,
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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    attributions: Vec<AttributionRange>,
}

#[derive(Serialize)]
struct LineTotals {
    e: usize,
    ai_direct: usize,
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

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AttributionRange {
    pub path: String,
    pub start_line: usize,
    pub end_line: usize,
    #[serde(default)]
    pub end_line_exclusive: Option<usize>,
    #[serde(default = "default_unknown_category")]
    pub category: String,
    #[serde(default)]
    pub score: Option<f64>,
    #[serde(default = "default_none_band")]
    pub confidence_band: String,
    #[serde(default)]
    pub primary_basis: Option<String>,
    #[serde(default)]
    pub supporting_basis: Vec<String>,
    #[serde(default)]
    pub limiting_basis: Vec<String>,
    #[serde(default)]
    pub source: Option<AttributionSource>,
    #[serde(default)]
    pub operation: Option<String>,
    #[serde(default = "default_algorithm_profile")]
    pub algorithm_profile: String,
    #[serde(default = "default_score_profile")]
    pub score_profile: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclusion_classifier: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclusion_reason: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AttributionSource {
    #[serde(default)]
    pub adapter_id: Option<String>,
    #[serde(default)]
    pub tool_id: Option<String>,
    #[serde(default)]
    pub session_ref: Option<String>,
}

fn default_algorithm_profile() -> String {
    "deterministic-v1".into()
}

fn default_unknown_category() -> String {
    "unknown".into()
}

fn default_none_band() -> String {
    "none".into()
}

fn default_score_profile() -> String {
    "score-v1".into()
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
    ai_direct: usize,
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
    old_path: Option<String>,
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
    tool: Option<String>,
    #[serde(default)]
    tool_use_id: Option<String>,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    window_open: Option<u128>,
    #[serde(default)]
    window_close: Option<u128>,
    #[serde(default)]
    operation: Option<String>,
    #[serde(default)]
    structured_edit: Option<bool>,
    #[serde(default)]
    line_ranges: Option<Vec<[usize; 2]>>,
    #[serde(default)]
    before_sha: Option<String>,
    #[serde(default)]
    after_sha: Option<String>,
    #[serde(default)]
    degraded: Option<bool>,
    #[serde(default)]
    overlap: Option<bool>,
    #[serde(default)]
    after_incomplete: Option<bool>,
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

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ExplainCheck {
    condition: String,
    passed: bool,
    compared: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ExplainLine {
    line: usize,
    category: String,
    score: Option<f64>,
    checks: Vec<ExplainCheck>,
    first_failed_condition: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ExplainReport {
    commit: String,
    file: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
    lines: Vec<ExplainLine>,
}

struct EventDecisionTrace {
    event: String,
    tool_use_id: Option<String>,
    operation: Option<String>,
    checks: Vec<ExplainCheck>,
}

pub fn derive_commit(repo_path: &Path, revision: &str) -> Result<PathBuf, String> {
    let (repo, repo_id, store) = repo_context(repo_path)?;
    let events = read_events(&store.join("events"));
    let event_index = index_events(&events);
    derive_commit_with_events(&repo, &repo_id, &store, revision, &events, &event_index)
}

pub fn reports_for_commits(repo_path: &Path, commits: &[String]) -> Result<CommitReports, String> {
    let (_, repo_id, store) = repo_context(repo_path)?;
    let reports = commits
        .iter()
        .map(
            |commit| match fs::read(store.join("commits").join(format!("{commit}.json"))) {
                Ok(bytes) => Ok(Some(bytes)),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(error) => Err(error.to_string()),
            },
        )
        .collect::<Result<Vec<_>, _>>()?;
    Ok(CommitReports { repo_id, reports })
}

pub fn lines_for_commits(
    repo_path: &Path,
    commits: &[String],
) -> Result<Vec<Option<Vec<u8>>>, String> {
    let (_, _, store) = repo_context(repo_path)?;
    commits
        .iter()
        .map(
            |commit| match fs::read(store.join("commits").join(format!("{commit}.lines.json"))) {
                Ok(bytes) => Ok(Some(bytes)),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(error) => Err(error.to_string()),
            },
        )
        .collect()
}

pub struct CommitReports {
    pub repo_id: String,
    pub reports: Vec<Option<Vec<u8>>>,
}

pub fn tools_for_commits(repo_path: &Path, commits: &[String]) -> Result<Vec<Vec<String>>, String> {
    let (repo, _, store) = repo_context(repo_path)?;
    let events = read_events(&store.join("events"));
    let mut result = Vec::with_capacity(commits.len());
    for commit in commits {
        let parents = git_checked(&repo.root, &["show", "-s", "--format=%P", commit])?;
        let parent = parents.split_whitespace().next();
        let start = parent
            .map(|parent| committer_time_ns(&repo.root, parent))
            .transpose()?
            .unwrap_or_default();
        let end = committer_time_ns(&repo.root, commit)?.saturating_add(2 * NANOSECOND);
        let (files, _) = commit_files(&repo.root, parent, commit)?;
        let changed_paths: BTreeSet<_> = files.into_iter().map(|file| file.path).collect();
        let tools: BTreeSet<_> = events
            .iter()
            .filter(|event| {
                event.event == "ai_change"
                    && event.timestamp >= start
                    && event.timestamp <= end
                    && event
                        .path
                        .as_deref()
                        .is_some_and(|path| changed_paths.contains(path))
            })
            .filter_map(|event| event.tool.clone())
            .collect();
        result.push(tools.into_iter().collect());
    }
    Ok(result)
}

pub fn enqueue_git_commits(repo_path: &Path, commits: &[String]) -> Result<bool, String> {
    if commits.is_empty() {
        return Ok(false);
    }
    let Some((root, git_dir, common_dir)) = crate::find_git_dirs(repo_path) else {
        return Ok(false);
    };
    let Some(home) = crate::store_home() else {
        return Ok(false);
    };
    if crate::normalized_path(&home).starts_with(crate::normalized_path(&root)) {
        return Ok(false);
    }

    let cache_path = home.join("repo-cache").join(format!(
        "{}.json",
        crate::hash(git_dir.to_string_lossy().as_bytes())
    ));
    let cached: Option<crate::RepoCache> = crate::read_json(&cache_path);
    let shallow = [git_dir.join("shallow"), common_dir.join("shallow")]
        .iter()
        .any(|path| fs::metadata(path).is_ok_and(|metadata| metadata.len() > 0));
    let git_dir_created_ns = fs::metadata(&git_dir)
        .ok()
        .and_then(|metadata| metadata.created().ok())
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos());

    let repo_id = if shallow {
        cached.map(|cache| cache.repo_id)
    } else {
        let cached_is_current = cached.as_ref().is_some_and(|cache| {
            if let Some(created) = git_dir_created_ns {
                cache.git_dir_created_ns == Some(created)
            } else {
                roots_check(&root).is_ok_and(|root_check| cache.root_check == root_check)
            }
        });
        if cached_is_current {
            cached.map(|cache| cache.repo_id)
        } else {
            let root_check = roots_check(&root)?;
            (!root_check.is_empty()).then(|| crate::hash(root_check.as_bytes()))
        }
    };
    let Some(repo_id) = repo_id else {
        return Ok(false);
    };
    let store = home.join("repos").join(&repo_id);
    let events_dir = store.join("events");
    if !events_dir.is_dir()
        || !fs::read_dir(&events_dir)
            .into_iter()
            .flatten()
            .flatten()
            .any(|entry| {
                entry
                    .path()
                    .extension()
                    .is_some_and(|extension| extension == "jsonl")
                    && entry.metadata().is_ok_and(|metadata| metadata.len() > 0)
            })
    {
        return Ok(false);
    }

    let mut unique = BTreeSet::new();
    let mut events = Vec::new();
    for commit in commits {
        let commit = commit.to_ascii_lowercase();
        if !matches!(commit.len(), 40 | 64)
            || !commit.bytes().all(|byte| byte.is_ascii_hexdigit())
            || !unique.insert(commit.clone())
        {
            continue;
        }
        let mut event =
            crate::Event::new("head_move", Some("git"), Some(&repo_id), Some("git"), None);
        event.new_head = Some(commit);
        event.head_moved = Some(true);
        event.commit_in_window = Some(false);
        events.push(event);
    }
    if events.is_empty() {
        return Ok(false);
    }
    Ok(crate::append_events(&events_dir.join("git.jsonl"), &events))
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
                ai_direct: 0,
                ai_strong: 0,
                ai_probable: 0,
                mixed: 0,
                unknown: 0,
            },
            files: Vec::new(),
            attributions: Vec::new(),
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
    let mut parent_line_labels = parent
        .as_deref()
        .map(|parent| read_parent_line_labels(store, parent))
        .unwrap_or_default();
    let (files, has_changes) = commit_files(&repo.root, parent.as_deref(), &commit)?;
    for file in &files {
        if let Some(old_path) = &file.old_path {
            let labels = parent_line_labels.get(old_path).cloned().or_else(|| {
                parent.as_deref().and_then(|parent| {
                    read_latest_path_line_labels(&repo.root, store, parent, old_path)
                })
            });
            if let Some(labels) = labels {
                parent_line_labels.insert(old_path.clone(), labels.clone());
                parent_line_labels.insert(file.path.clone(), labels);
            }
        }
    }
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
    let mut attributions = Vec::new();
    let mut parent_fallback_lines = HashMap::<String, usize>::new();
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
        let path_exclusion = crate::exclusion::classify_path(&file.path);
        if path_exclusion.is_some_and(|exclusion| exclusion.reason_code != "BINARY_FILE") {
            let exclusion = path_exclusion.expect("checked path exclusion");
            attributions.extend(excluded_attributions(
                &file.path,
                &parent_content,
                &committed_content,
                exclusion.reason_code,
                exclusion.limiting_basis,
            ));
            output_files.push(skipped_file(
                file.path,
                &parent_content,
                &committed_content,
                "skipped_excluded",
            ));
            continue;
        }
        let binary_content = parent_content[..parent_content.len().min(8192)].contains(&0)
            || committed_content[..committed_content.len().min(8192)].contains(&0);
        let content_exclusion = if binary_content {
            crate::exclusion::classify_content(&committed_content)
                .or_else(|| crate::exclusion::classify_content(&parent_content))
        } else {
            crate::exclusion::classify_content(&committed_content)
        };
        if let Some(exclusion) = content_exclusion
            .or_else(|| path_exclusion.filter(|exclusion| exclusion.reason_code == "BINARY_FILE"))
        {
            attributions.extend(excluded_attributions(
                &file.path,
                &parent_content,
                &committed_content,
                exclusion.reason_code,
                exclusion.limiting_basis,
            ));
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
            let exclusion = crate::exclusion::Exclusion {
                reason_code: crate::exclusion::FILE_SIZE_LIMIT_REASON,
                limiting_basis: None,
            };
            attributions.extend(excluded_attributions(
                &file.path,
                &parent_content,
                &committed_content,
                exclusion.reason_code,
                exclusion.limiting_basis,
            ));
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
        let (mut steps, mut evidence) = steps_for_events(
            store,
            &selected_events,
            &parent_content,
            &normalized_committed,
            commit_time,
        );
        if evidence == "missing_blob" {
            if let Some(fallback) = missing_snapshot_time_cut_fallback(
                store,
                &selected_events,
                &committed_content,
                &normalized_committed,
            ) {
                steps = vec![fallback];
                evidence = "time_cut";
            } else {
                unreadable_files += 1;
                attributions.extend(unknown_attributions(
                    &file.path,
                    &normalized_parent,
                    &normalized_committed,
                ));
                output_files.push(unknown_file(file.path, evidence, added_count));
                continue;
            }
        }

        if (reconstruct_all || reconstruct_paths.contains(file.path.as_str()))
            && steps.last().map(|step| step.after.as_slice())
                != Some(normalized_committed.as_slice())
        {
            steps.push(Step {
                kind: StepKind::Ai { degraded: true },
                after: normalized_committed.clone(),
                attribution: Some(derive::StepAttribution {
                    adapter_id: None,
                    tool_id: None,
                    run_id: None,
                    session_id: None,
                    operation: None,
                    primary_basis: None,
                    score_milli: 600,
                    penalties_milli: Vec::new(),
                    supporting_basis: Vec::new(),
                    limiting_basis: vec!["X_RECONSTRUCTED_WINDOW".into()],
                    direct_lines: Vec::new(),
                }),
            });
            evidence = "reconstructed";
        }
        let survival_steps = if evidence == "time_cut" {
            prepare_unobserved_survival_steps(&steps)
        } else {
            None
        };
        let use_unobserved_survival = survival_steps.is_some();
        let mut evidence_input = Evidence {
            parent: normalized_parent.clone(),
            steps: survival_steps.unwrap_or(steps),
        };
        let mut attributed_lines = if use_unobserved_survival {
            derive::derive_lines_with_unobserved_edits(&evidence_input, &normalized_committed)
        } else {
            derive::derive_lines_with_attribution(&evidence_input, &normalized_committed)
        };
        let committed_line_bytes = split_normalized_lines(&normalized_committed);
        let parent_lines = parent_line_labels.get(&file.path);
        let fallback_matches = parent_lines
            .map(|labels| {
                parent_lineage_fallback_matches(&normalized_parent, &normalized_committed, labels)
            })
            .unwrap_or_default();
        if !fallback_matches.is_empty() {
            for line in &mut attributed_lines {
                if line.line.label == derive::Label::Unknown {
                    line.source_step = None;
                }
            }
        }
        let mut fallback_count = 0;
        for (line_index, parent_line) in fallback_matches {
            let Some(attributed) = attributed_lines.get_mut(line_index) else {
                continue;
            };
            if !attributed.line.added
                || attributed.line.blank
                || attributed.line.label != derive::Label::Unknown
            {
                continue;
            }

            let score_milli = parent_line.score.map_or_else(
                || default_parent_score_milli(&parent_line.category),
                |score| (score.clamp(0.0, 1.0) * 1000.0).round() as u16,
            );
            let step_index = evidence_input.steps.len();
            evidence_input.steps.push(Step {
                kind: StepKind::Unknown,
                after: Vec::new(),
                attribution: Some(derive::StepAttribution {
                    adapter_id: parent_line
                        .source
                        .as_ref()
                        .and_then(|source| source.adapter_id.clone()),
                    tool_id: parent_line
                        .source
                        .as_ref()
                        .and_then(|source| source.tool_id.clone()),
                    run_id: parent_line
                        .source
                        .as_ref()
                        .and_then(|source| source.session_ref.clone()),
                    session_id: None,
                    operation: parent_line.operation,
                    primary_basis: Some("CONFLICTING_EVIDENCE".into()),
                    score_milli,
                    penalties_milli: Vec::new(),
                    supporting_basis: parent_line.supporting_basis,
                    limiting_basis: vec!["X_UNOBSERVED_EDIT_IN_FILE".into()],
                    direct_lines: Vec::new(),
                }),
            });
            attributed.line.label = derive::Label::Mixed;
            attributed.source_step = Some(step_index);
            attributed.ambiguous = false;
            fallback_count += 1;
        }
        if use_unobserved_survival {
            for hunk in derive::unobserved_replacement_hunks(&evidence_input, &normalized_committed)
            {
                let old_text = hunk
                    .removed_ai
                    .iter()
                    .map(|line| line.text.as_slice())
                    .collect::<Vec<_>>();
                let new_indices = hunk
                    .added_line_indices
                    .into_iter()
                    .filter(|&index| {
                        attributed_lines.get(index).is_some_and(|line| {
                            line.line.added
                                && !line.line.blank
                                && line.line.label == derive::Label::Unknown
                        })
                    })
                    .collect::<Vec<_>>();
                let new_text = new_indices
                    .iter()
                    .map(|&index| committed_line_bytes[index])
                    .collect::<Vec<_>>();
                for (old_match, new_match) in
                    order_preserving_similarity_matches(&old_text, &new_text)
                {
                    let Some(source_index) = hunk.removed_ai[old_match].source_step else {
                        continue;
                    };
                    let Some(source_attribution) = evidence_input
                        .steps
                        .get(source_index)
                        .and_then(|step| step.attribution.as_ref())
                    else {
                        continue;
                    };
                    let mut attribution = source_attribution.clone();
                    attribution.score_milli =
                        default_parent_score_milli(category_name(hunk.removed_ai[old_match].label));
                    attribution.penalties_milli.clear();
                    attribution
                        .limiting_basis
                        .retain(|basis| basis != "CONFLICTING_EVIDENCE");
                    push_basis(&mut attribution.limiting_basis, "X_UNOBSERVED_EDIT_IN_FILE");
                    let mixed_step = evidence_input.steps.len();
                    evidence_input.steps.push(Step {
                        kind: StepKind::Unknown,
                        after: Vec::new(),
                        attribution: Some(attribution),
                    });
                    let attributed = &mut attributed_lines[new_indices[new_match]];
                    attributed.line.label = derive::Label::Mixed;
                    attributed.source_step = Some(mixed_step);
                    attributed.ambiguous = false;
                    attributed.unobserved_edit =
                        Some(derive::UnobservedEditKind::MixedBySimilarity);
                    fallback_count += 1;
                }
            }
        }
        if fallback_count > 0 {
            parent_fallback_lines.insert(file.path.clone(), fallback_count);
        }
        let labels: Vec<_> = attributed_lines
            .iter()
            .map(|line| line.line.clone())
            .collect();
        let counts = derive::count(&labels);
        let lines = attributed_lines
            .iter()
            .filter(|line| line.line.added && !line.line.blank)
            .filter_map(|line| {
                let text = committed_line_bytes.get(line.line.line.checked_sub(1)?)?;
                let digest = crate::hash(text);
                Some(LineOutput {
                    line: line.line.line,
                    hash: digest[..16].to_owned(),
                    label: match line.line.label {
                        derive::Label::AiDirect => 'd',
                        derive::Label::AiStrong => 's',
                        derive::Label::AiProbable => 'p',
                        derive::Label::Mixed => 'm',
                        derive::Label::Unknown => 'u',
                    },
                })
            })
            .collect();
        attributions.extend(attribution_ranges(
            &file.path,
            &attributed_lines,
            &evidence_input.steps,
        ));
        if let Some(old_path) = &file.old_path {
            if let Some(labels) = parent_line_labels.get(old_path) {
                attributions.extend(rename_line_attributions(
                    &file.path,
                    &parent_content,
                    &committed_content,
                    labels,
                ));
            }
        }
        line_files.insert(file.path.clone(), lines);
        output_files.push(FileOutput {
            path: file.path,
            e: counts.e,
            ai_direct: counts.d,
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
    attributions.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then_with(|| left.start_line.cmp(&right.start_line))
    });
    let mut totals = Totals::default();
    let mut covered = 0usize;
    for file in &output_files {
        totals.e += file.e;
        totals.ai_direct += file.ai_direct;
        totals.ai_strong += file.ai_strong;
        totals.ai_probable += file.ai_probable;
        totals.mixed += file.mixed;
        totals.unknown += file.unknown;
        totals.excluded += file.excluded;
        if file.evidence != "none" && file.evidence != "missing_blob" {
            covered += file.e.saturating_sub(
                parent_fallback_lines
                    .get(&file.path)
                    .copied()
                    .unwrap_or_default(),
            );
        }
    }
    totals.unreadable_files = unreadable_files;
    totals.ai_deterministic_pct = percentage_tenths(totals.ai_direct + totals.ai_strong, totals.e);
    totals.ai_linked_pct = percentage_tenths(
        totals.ai_direct + totals.ai_strong + totals.ai_probable + totals.mixed,
        totals.e,
    );
    let coverage_unobserved_lines = unobserved_eligible_line_count(&attributions);
    let mut coverage = coverage_band_with_unobserved(covered, totals.e, coverage_unobserved_lines);
    if !parent_fallback_lines.is_empty() && coverage == "high" {
        coverage = "partial";
    }
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
            ai_direct: totals.ai_direct,
            ai_strong: totals.ai_strong,
            ai_probable: totals.ai_probable,
            mixed: totals.mixed,
            unknown: totals.unknown,
        },
        files: sidecar_files,
        attributions,
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
        coverage_unobserved_lines,
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
            ai_direct: 0,
            ai_strong: 0,
            ai_probable: 0,
            mixed: 0,
            unknown: 0,
        },
        files: Vec::new(),
        attributions: Vec::new(),
    })
    .map_err(|error| error.to_string())?;
    let bytes = serde_json::to_vec(&CommitOutput {
        schema: "prov.commit/0.1",
        repo_id: repo_id.to_owned(),
        commit,
        parent,
        totals: Totals::default(),
        coverage: "unknown",
        coverage_unobserved_lines: 0,
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
    let diff = git_output(root, &["diff-tree", "-p", "-M", parent, commit]).ok()?;
    if !diff.status.success() || !patch_diff_size_allowed(diff.stdout.len()) {
        return None;
    }
    let mut command = Command::new("git");
    command
        .args(["patch-id", "--stable"])
        .current_dir(root)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    crate::configure_process_command(&mut command, false);
    let mut child = command.spawn().ok()?;
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

/// Explain attribution for a committed file without writing to the provenance store.
pub fn explain_commit(
    repo_path: &Path,
    revision: &str,
    file_path: &str,
    line_filter: Option<usize>,
) -> Result<serde_json::Value, String> {
    let (root, _repo_id, store) = read_only_repo_context(repo_path)?;
    let commit = resolve_commit(&root, revision)?;
    let parent_line = git_checked(&root, &["rev-list", "--parents", "-n", "1", &commit])?;
    let commits: Vec<_> = parent_line.split_whitespace().collect();
    if commits.first() != Some(&commit.as_str()) || commits.len() > 2 {
        return Err("explain requires a non-merge commit".into());
    }
    let parent = commits.get(1).copied();
    let (files, has_changes) = commit_files(&root, parent, &commit)?;
    if !has_changes {
        return Err("commit has no changed files".into());
    }
    let path = file_path.replace('\\', "/");
    let file = files
        .into_iter()
        .find(|candidate| candidate.path == path)
        .ok_or_else(|| format!("file {path} is not changed by commit {commit}"))?;
    let mut blob_ids = vec![file.new_blob.clone()];
    if let Some(old) = &file.old_blob {
        blob_ids.push(old.clone());
    }
    let sizes = crate::check_blob_sizes(&root, &blob_ids).ok_or("could not read Git blob sizes")?;
    if sizes.values().any(|size| *size > MAX_FILE) {
        return Err("cannot explain a file larger than the attribution limit".into());
    }
    let contents = crate::cat_blobs(&root, &blob_ids).ok_or("could not read Git blobs")?;
    let parent_content = match &file.old_blob {
        Some(old) => contents
            .get(old)
            .cloned()
            .ok_or("could not read parent file content")?,
        None => Vec::new(),
    };
    let committed_content = contents
        .get(&file.new_blob)
        .cloned()
        .ok_or("could not read committed file content")?;

    let path_exclusion = crate::exclusion::classify_path(&path);
    let path_restricted =
        path_exclusion.is_some_and(|exclusion| exclusion.reason_code != "BINARY_FILE");
    let binary_content = parent_content[..parent_content.len().min(8192)].contains(&0)
        || committed_content[..committed_content.len().min(8192)].contains(&0);
    let content_exclusion = if path_restricted {
        None
    } else if binary_content {
        crate::exclusion::classify_content(&committed_content)
            .or_else(|| crate::exclusion::classify_content(&parent_content))
    } else {
        crate::exclusion::classify_content(&committed_content)
    };
    let exclusion = if path_restricted {
        path_exclusion
    } else {
        content_exclusion
            .or_else(|| path_exclusion.filter(|exclusion| exclusion.reason_code == "BINARY_FILE"))
    };
    let normalized_parent = normalize_eol(&parent_content);
    let normalized_committed = normalize_eol(&committed_content);
    if let Some(exclusion) = exclusion {
        let attributions = excluded_attributions(
            &path,
            &parent_content,
            &committed_content,
            exclusion.reason_code,
            exclusion.limiting_basis,
        );
        let lines = attributions
            .iter()
            .flat_map(|range| range.start_line..=range.end_line)
            .filter(|line| line_filter.is_none_or(|wanted| wanted == *line))
            .map(|line| ExplainLine {
                line,
                category: "excluded".into(),
                score: None,
                checks: vec![explain_check(
                    "exclusion_rules",
                    false,
                    format!("excluded: {}", exclusion.reason_code),
                )],
                first_failed_condition: Some("exclusion_rules".into()),
            })
            .collect();
        return serde_json::to_value(ExplainReport {
            commit,
            file: path,
            message: None,
            lines,
        })
        .map_err(|error| error.to_string());
    }

    let parent_time = parent
        .map(|value| committer_time_ns(&root, value))
        .transpose()?
        .unwrap_or(0);
    let commit_time = committer_time_ns(&root, &commit)?;
    let events = read_events(&store.join("events"));
    let event_index = index_events(&events);
    let selected_events = events_for_path(
        &store,
        &events,
        &event_index,
        &path,
        parent_time,
        &normalized_parent,
    );
    let (mut steps, mut evidence, event_traces) = steps_for_events_with_trace(
        &store,
        &selected_events,
        &parent_content,
        &normalized_committed,
        commit_time,
    );
    if evidence == "missing_blob" {
        if let Some(fallback) = missing_snapshot_time_cut_fallback(
            &store,
            &selected_events,
            &committed_content,
            &normalized_committed,
        ) {
            steps = vec![fallback];
            evidence = "time_cut";
        } else {
            return Err(
                "snapshot evidence is missing; derivation would mark this file unknown".into(),
            );
        }
    }
    let survival_steps = if evidence == "time_cut" {
        prepare_unobserved_survival_steps(&steps)
    } else {
        None
    };
    let evidence_input = Evidence {
        parent: normalized_parent,
        steps: survival_steps.unwrap_or(steps),
    };
    let attributed_lines = if evidence == "time_cut" {
        derive::derive_lines_with_unobserved_edits(&evidence_input, &normalized_committed)
    } else {
        derive::derive_lines_with_attribution(&evidence_input, &normalized_committed)
    };
    let ranges = attribution_ranges(&path, &attributed_lines, &evidence_input.steps);
    let stored_ranges = fs::read(store.join("commits").join(format!("{commit}.lines.json")))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .and_then(|value| {
            value
                .get("attributions")
                .and_then(serde_json::Value::as_array)
                .cloned()
        })
        .unwrap_or_default();
    let report_lines = attributed_lines
        .iter()
        .filter(|attributed| attributed.line.added && !attributed.line.blank)
        .filter(|attributed| line_filter.is_none_or(|wanted| wanted == attributed.line.line))
        .map(|attributed| {
            let stored_range = stored_ranges.iter().find(|range| {
                range["path"] == path
                    && range["startLine"]
                        .as_u64()
                        .is_some_and(|start| start as usize <= attributed.line.line)
                    && range["endLine"]
                        .as_u64()
                        .is_some_and(|end| attributed.line.line <= end as usize)
            });
            let range = ranges.iter().find(|range| {
                range.start_line <= attributed.line.line && attributed.line.line <= range.end_line
            });
            let category = stored_range
                .and_then(|range| range["category"].as_str())
                .map(str::to_owned)
                .or_else(|| range.map(|range| range.category.clone()))
                .unwrap_or_else(|| category_name(attributed.line.label).to_owned());
            let score = stored_range
                .and_then(|range| range["score"].as_f64())
                .or_else(|| range.and_then(|range| range.score));
            let source_trace = attributed
                .source_step
                .and_then(|index| event_traces.get(index));
            let mut checks = source_trace
                .map(|trace| {
                    let mut checks = trace.checks.clone();
                    checks.push(explain_check(
                        "source_event",
                        true,
                        format!(
                            "event={}; tool_use_id={:?}; operation={:?}",
                            trace.event, trace.tool_use_id, trace.operation
                        ),
                    ));
                    checks
                })
                .unwrap_or_else(no_event_checks);
            checks.push(explain_check(
                "survival",
                attributed.line.label != derive::Label::Unknown && attributed.source_step.is_some(),
                format!(
                    "source_step={:?}; final_category={category}",
                    attributed.source_step
                ),
            ));
            checks.push(explain_check(
                "duplicate_ambiguity",
                !attributed.ambiguous,
                format!("ambiguous={}", attributed.ambiguous),
            ));
            checks.push(explain_check(
                "unobserved_edit_in_file",
                attributed.unobserved_edit.is_none(),
                format!("unobserved_edit={:?}", attributed.unobserved_edit),
            ));
            checks.push(explain_check(
                "exclusion_rules",
                true,
                "file passed path and content exclusion checks".into(),
            ));
            let first_failed_condition = checks
                .iter()
                .find(|check| !check.passed)
                .map(|check| check.condition.clone());
            ExplainLine {
                line: attributed.line.line,
                category,
                score,
                checks,
                first_failed_condition,
            }
        })
        .collect();
    let message = line_filter
        .filter(|line| {
            split_normalized_lines(&normalized_committed)
                .get(line.saturating_sub(1))
                .is_some_and(|text| text.iter().all(u8::is_ascii_whitespace))
        })
        .map(|_| "blank line, not eligible".to_owned());
    serde_json::to_value(ExplainReport {
        commit,
        file: path,
        message,
        lines: report_lines,
    })
    .map_err(|error| error.to_string())
}

pub fn explain_table(value: &serde_json::Value) -> Result<String, String> {
    let commit = value["commit"]
        .as_str()
        .ok_or("missing commit in explain result")?;
    let file = value["file"]
        .as_str()
        .ok_or("missing file in explain result")?;
    let lines = value["lines"]
        .as_array()
        .ok_or("missing lines in explain result")?;
    if lines.is_empty() {
        if let Some(message) = value["message"].as_str() {
            return Ok(format!("Commit {commit}  File {file}\n{message}\n"));
        }
    }
    let mut output =
        format!("Commit {commit}  File {file}\nLINE  CATEGORY      SCORE  FIRST FAILED\n");
    for line in lines {
        let number = line["line"].as_u64().unwrap_or_default();
        let category = line["category"].as_str().unwrap_or("unknown");
        let score = line["score"]
            .as_f64()
            .map(|score| format!("{score:.2}"))
            .unwrap_or_else(|| "—".into());
        let first_failed = line["firstFailedCondition"].as_str().unwrap_or("—");
        output.push_str(&format!(
            "{number:<5} {category:<13} {score:<6} {first_failed}\n"
        ));
        if let Some(checks) = line["checks"].as_array() {
            for check in checks {
                let status = if check["passed"] == true {
                    "PASS"
                } else {
                    "FAIL"
                };
                output.push_str(&format!(
                    "      {status:<4} {}: {}\n",
                    check["condition"].as_str().unwrap_or("condition"),
                    check["compared"].as_str().unwrap_or_default()
                ));
            }
        }
    }
    Ok(output)
}

fn no_event_checks() -> Vec<ExplainCheck> {
    [
        "structured_edit",
        "before_sha",
        "after_sha",
        "line_ranges",
        "overlap",
        "degraded",
        "after_incomplete",
        "evidence_exactness",
    ]
    .into_iter()
    .map(|condition| {
        let passed = matches!(condition, "overlap" | "degraded" | "after_incomplete");
        explain_check(condition, passed, "no matching event".into())
    })
    .collect()
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
        value["coverage"].as_str().unwrap_or("unknown"),
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

fn read_only_repo_context(path: &Path) -> Result<(PathBuf, String, PathBuf), String> {
    let home = crate::store_home().ok_or("could not determine store directory")?;
    let (root, _, _) = crate::find_git_dirs(path).ok_or("not a supported Git repository")?;
    if crate::normalized_path(&home).starts_with(crate::normalized_path(&root)) {
        return Err("the provenance store must be outside the repository".into());
    }
    let roots = roots_check(&root)?;
    if roots.is_empty() {
        return Err("repository identity unavailable".into());
    }
    let repo_id = crate::hash(roots.as_bytes());
    let store = home.join("repos").join(&repo_id);
    Ok((root, repo_id, store))
}

fn roots_check(root: &Path) -> Result<String, String> {
    let roots = git_checked(
        root,
        &["rev-list", "--first-parent", "--max-parents=0", "HEAD"],
    )?;
    let mut roots: Vec<_> = roots.lines().map(str::to_owned).collect();
    roots.sort();
    Ok(roots.join("\n"))
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
                "-M",
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
                "-M",
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
        let Some(first_path) = records.next() else {
            break;
        };
        let fields: Vec<_> = header.split(|byte| *byte == b' ').collect();
        if fields.len() < 5 || fields[4].first() == Some(&b'D') {
            continue;
        }
        let is_rename = fields[4].first() == Some(&b'R');
        let (old_path, path) = if is_rename {
            let Some(new_path) = records.next() else {
                break;
            };
            (
                Some(String::from_utf8_lossy(first_path).replace('\\', "/")),
                new_path,
            )
        } else {
            (None, first_path)
        };
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
        let path = String::from_utf8_lossy(path).replace('\\', "/");
        files.push(CommitFile {
            path: path.clone(),
            old_path,
            old_blob,
            new_blob: new_id,
            added_lines: additions.get(&path).copied().unwrap_or_default(),
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
            "-M",
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
            "-M",
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
    let mut records = output.stdout.split(|byte| *byte == 0);
    while let Some(record) = records.next() {
        if record.is_empty() {
            continue;
        }
        let fields: Vec<_> = record.splitn(3, |byte| *byte == b'\t').collect();
        if fields.len() != 3 || fields[0] == b"-" {
            continue;
        }
        let Ok(count) = std::str::from_utf8(fields[0]).unwrap_or_default().parse() else {
            continue;
        };
        let path = if fields[2].is_empty() {
            let Some(_old_path) = records.next() else {
                break;
            };
            let Some(new_path) = records.next() else {
                break;
            };
            new_path
        } else {
            fields[2]
        };
        additions.insert(String::from_utf8_lossy(path).replace('\\', "/"), count);
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
    normalize_window_overlaps(&mut events);
    events
}

fn normalize_window_overlaps(events: &mut [EventRecord]) {
    let mut starts = HashMap::<(String, String), u128>::new();
    let mut intervals = Vec::<(String, String, u128, u128)>::new();
    for event in events.iter() {
        let (Some(session), Some(tool_use_id)) =
            (event.session_id.as_ref(), event.tool_use_id.as_ref())
        else {
            continue;
        };
        let key = (session.clone(), tool_use_id.clone());
        match event.event.as_str() {
            "window_open" => {
                if let Some(opened_at) = event.window_open {
                    starts.insert(key, opened_at);
                }
            }
            "window_close" => {
                if let (Some(start), Some(end)) = (starts.remove(&key), event.window_close) {
                    if end >= start {
                        intervals.push((session.clone(), tool_use_id.clone(), start, end));
                    }
                }
            }
            _ => {}
        }
    }
    let mut overlapping = HashSet::<(String, String)>::new();
    for (index, (session, tool, start, end)) in intervals.iter().enumerate() {
        for (other_session, other_tool, other_start, other_end) in intervals.iter().skip(index + 1)
        {
            if session != other_session && start < other_end && other_start < end {
                overlapping.insert((session.clone(), tool.clone()));
                overlapping.insert((other_session.clone(), other_tool.clone()));
            }
        }
    }
    for event in events.iter_mut().filter(|event| event.event == "ai_change") {
        event.overlap = Some(
            event
                .session_id
                .as_ref()
                .zip(event.tool_use_id.as_ref())
                .is_some_and(|key| overlapping.contains(&(key.0.clone(), key.1.clone()))),
        );
    }
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
    parent: &[u8],
    committed: &[u8],
    commit_time: u128,
) -> (Vec<Step>, &'static str) {
    let (steps, evidence, _) =
        steps_for_events_with_trace(store, events, parent, committed, commit_time);
    (steps, evidence)
}

fn steps_for_events_with_trace(
    store: &Path,
    events: &[&EventRecord],
    parent: &[u8],
    committed: &[u8],
    commit_time: u128,
) -> (Vec<Step>, &'static str, Vec<EventDecisionTrace>) {
    if events.is_empty() {
        return (Vec::new(), "none", Vec::new());
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
            return (Vec::new(), "none", Vec::new());
        }
        (filtered, "time_cut")
    };
    let evidence_comparison = exact_end.map_or_else(
        || "no stored after snapshot normalized to the committed bytes".to_owned(),
        |end| {
            let event = events[end];
            let raw = event
                .after_sha
                .as_deref()
                .and_then(|sha| crate::read_blob(&store.join("blobs").join(sha), sha));
            format!(
                "matching_event_index={end}; event_after_sha={:?}; normalized_snapshot_sha={:?}; normalized_committed_sha={}; snapshot_matches_committed={}",
                event.after_sha,
                raw.as_ref().map(|bytes| crate::hash(&normalize_eol(bytes))),
                crate::hash(committed),
                raw.as_ref().is_some_and(|bytes| normalize_eol(bytes) == committed)
            )
        },
    );

    let mut steps = Vec::with_capacity(selected.len());
    let mut decision_traces = Vec::with_capacity(selected.len());
    let mut previous_snapshot = normalize_eol(parent);
    let mut previous_snapshot_raw = parent.to_vec();
    for event in selected {
        let (after_snapshot, after) = if event.event == "deletion" || event.after_sha.is_none() {
            (Vec::new(), Vec::new())
        } else {
            let sha = event.after_sha.as_deref().unwrap_or_default();
            if !is_sha256(sha) {
                decision_traces.push(missing_snapshot_trace(
                    store,
                    event,
                    &previous_snapshot_raw,
                    &previous_snapshot,
                    evidence,
                    "invalid after_sha format",
                ));
                return (Vec::new(), "missing_blob", decision_traces);
            }
            let Some(bytes) = crate::read_blob(&store.join("blobs").join(sha), sha) else {
                decision_traces.push(missing_snapshot_trace(
                    store,
                    event,
                    &previous_snapshot_raw,
                    &previous_snapshot,
                    evidence,
                    "after snapshot blob is unavailable or fails its raw content hash",
                ));
                return (Vec::new(), "missing_blob", decision_traces);
            };
            (bytes.clone(), normalize_eol(&bytes))
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
        let mut limiting_basis = Vec::new();
        if evidence == "time_cut" {
            limiting_basis.push("X_TIME_CUT".into());
        }
        if event.overlap.unwrap_or(false) {
            limiting_basis.push("X_OVERLAPPING_SESSIONS".into());
        }
        if event.degraded.unwrap_or(false) || event.after_incomplete.unwrap_or(false) {
            limiting_basis.push("X_DEGRADED".into());
            limiting_basis.push("INCOMPLETE_SOURCE".into());
        }
        if event.tool_use_id.is_none() {
            limiting_basis.push("INCOMPLETE_SOURCE".into());
        }
        let mut penalties_milli = Vec::new();
        if evidence == "time_cut" {
            penalties_milli.push(120);
        }
        if event.degraded.unwrap_or(false) || event.after_incomplete.unwrap_or(false) {
            penalties_milli.push(100);
        }
        if event.tool_use_id.is_none() {
            penalties_milli.push(80);
        }
        let is_ai = matches!(kind, StepKind::Ai { .. });
        let before_snapshot = event
            .before_sha
            .as_deref()
            .and_then(|sha| crate::read_blob(&store.join("blobs").join(sha), sha))
            .filter(|raw| normalize_eol(raw) == previous_snapshot)
            .or_else(|| {
                (normalize_eol(&previous_snapshot_raw) == previous_snapshot)
                    .then(|| previous_snapshot_raw.clone())
            })
            .or_else(|| {
                (event.before_sha.is_none() && previous_snapshot.is_empty()).then(Vec::new)
            });
        let (direct_lines, checks) = if is_ai {
            verified_structured_lines(
                event,
                before_snapshot.as_deref(),
                &previous_snapshot,
                &after_snapshot,
                &after,
                evidence == "exact",
                &evidence_comparison,
            )
        } else {
            (Vec::new(), Vec::new())
        };
        decision_traces.push(EventDecisionTrace {
            event: event.event.clone(),
            tool_use_id: event.tool_use_id.clone(),
            operation: event.operation.clone(),
            checks,
        });
        let attribution = Some(derive::StepAttribution {
            adapter_id: event.tool.clone(),
            tool_id: event.tool.clone(),
            run_id: event.tool_use_id.clone(),
            session_id: event.session_id.clone(),
            operation: event.operation.clone(),
            primary_basis: Some(
                if !direct_lines.is_empty() {
                    if event.operation.as_deref() == Some("apply_patch") {
                        "DIRECT_AGENT_PATCH"
                    } else {
                        "DIRECT_ACCEPTED_EDIT"
                    }
                } else if is_ai {
                    "EXACT_FRAGMENT_MATCH"
                } else {
                    "INCOMPLETE_SOURCE"
                }
                .into(),
            ),
            score_milli: if !direct_lines.is_empty() {
                if event.operation.as_deref() == Some("apply_patch") {
                    990
                } else {
                    1000
                }
            } else if is_ai {
                920
            } else {
                0
            },
            penalties_milli,
            supporting_basis: Vec::new(),
            limiting_basis,
            direct_lines,
        });
        steps.push(Step {
            kind,
            after: after.clone(),
            attribution,
        });
        previous_snapshot = after;
        previous_snapshot_raw = after_snapshot;
    }
    (steps, evidence, decision_traces)
}

fn verified_structured_lines(
    event: &EventRecord,
    raw_before: Option<&[u8]>,
    normalized_before: &[u8],
    after_snapshot: &[u8],
    normalized_after: &[u8],
    evidence_exact: bool,
    evidence_comparison: &str,
) -> (Vec<Vec<u8>>, Vec<ExplainCheck>) {
    let before_actual = raw_before.map(crate::hash);
    let before_sha_matches = match (event.before_sha.as_deref(), before_actual.as_deref()) {
        (None, Some(actual)) => normalized_before.is_empty() && actual == crate::hash(&[]),
        (Some(expected), Some(actual)) => expected == actual,
        _ => false,
    };
    let after_actual = crate::hash(after_snapshot);
    let after_sha_matches = event.after_sha.as_deref() == Some(after_actual.as_str());
    let actual_ranges = crate::changed_line_ranges(normalized_before, normalized_after);
    let recorded_ranges = event.line_ranges.as_ref();
    let ranges_match = recorded_ranges == Some(&actual_ranges);
    let checks = vec![
        explain_check(
            "structured_edit",
            event.structured_edit == Some(true),
            format!("recorded={:?}; required=true", event.structured_edit),
        ),
        explain_check(
            "before_sha",
            before_sha_matches
                && raw_before.is_some_and(|bytes| normalize_eol(bytes) == normalized_before),
            format!(
                "raw_before_sha={:?}; computed_raw_sha={:?}; raw_snapshot_normalizes_to_previous={}; bytes={}",
                event.before_sha,
                before_actual,
                raw_before.is_some_and(|bytes| normalize_eol(bytes) == normalized_before),
                if raw_before.is_some() {
                    "available"
                } else {
                    "unavailable"
                }
            ),
        ),
        explain_check(
            "after_sha",
            after_sha_matches,
            format!(
                "raw_after_sha={:?}; computed_raw_sha={}; normalized_after_sha={}",
                event.after_sha,
                after_actual,
                crate::hash(normalized_after)
            ),
        ),
        explain_check(
            "line_ranges",
            ranges_match,
            format!(
                "recorded={:?}; recomputed_from_normalized_bytes={:?}",
                recorded_ranges, actual_ranges
            ),
        ),
        explain_check(
            "overlap",
            !event.overlap.unwrap_or(false),
            format!("recorded={:?}; required=false", event.overlap),
        ),
        explain_check(
            "degraded",
            !event.degraded.unwrap_or(false),
            format!("recorded={:?}; required=false", event.degraded),
        ),
        explain_check(
            "after_incomplete",
            !event.after_incomplete.unwrap_or(false),
            format!("recorded={:?}; required=false", event.after_incomplete),
        ),
        explain_check(
            "evidence_exactness",
            evidence_exact,
            evidence_comparison.to_owned(),
        ),
    ];
    if checks.iter().any(|check| !check.passed) {
        return (Vec::new(), checks);
    }
    let old = commit_split_lines(normalized_before);
    let new = commit_split_lines(normalized_after);
    let added = capture_diff_slices(Algorithm::Myers, &old, &new)
        .iter()
        .filter(|op| op.tag() != DiffTag::Equal)
        .flat_map(|op| op.new_range().map(|index| new[index].clone()))
        .collect::<Vec<_>>();
    let nonempty = !added.is_empty();
    let mut checks = checks;
    checks.push(explain_check(
        "verified_added_lines",
        nonempty,
        format!("verified_added_line_count={}", added.len()),
    ));
    if nonempty {
        (added, checks)
    } else {
        (Vec::new(), checks)
    }
}

fn explain_check(condition: &str, passed: bool, compared: String) -> ExplainCheck {
    ExplainCheck {
        condition: condition.to_owned(),
        passed,
        compared,
    }
}

fn missing_snapshot_trace(
    store: &Path,
    event: &EventRecord,
    previous_raw: &[u8],
    previous_normalized: &[u8],
    evidence: &str,
    after_status: &str,
) -> EventDecisionTrace {
    let before_raw = event
        .before_sha
        .as_deref()
        .and_then(|sha| crate::read_blob(&store.join("blobs").join(sha), sha))
        .filter(|bytes| normalize_eol(bytes) == previous_normalized)
        .or_else(|| {
            (normalize_eol(previous_raw) == previous_normalized).then(|| previous_raw.to_vec())
        });
    let before_actual = before_raw.as_deref().map(crate::hash);
    let before_ok = match (event.before_sha.as_deref(), before_actual.as_deref()) {
        (None, Some(actual)) => previous_normalized.is_empty() && actual == crate::hash(&[]),
        (Some(expected), Some(actual)) => expected == actual,
        _ => false,
    };
    let checks = vec![
        explain_check(
            "structured_edit",
            event.structured_edit == Some(true),
            format!("recorded={:?}; required=true", event.structured_edit),
        ),
        explain_check(
            "before_sha",
            before_ok,
            format!(
                "raw_before_sha={:?}; computed_raw_sha={:?}; raw_snapshot_normalizes_to_previous={}",
                event.before_sha,
                before_actual,
                before_raw
                    .as_deref()
                    .is_some_and(|bytes| normalize_eol(bytes) == previous_normalized)
            ),
        ),
        explain_check(
            "after_sha",
            false,
            format!(
                "raw_after_sha={:?}; computed_raw_sha=unavailable; {after_status}",
                event.after_sha
            ),
        ),
        explain_check(
            "line_ranges",
            false,
            format!(
                "recorded={:?}; recomputed ranges unavailable without the snapshot",
                event.line_ranges
            ),
        ),
        explain_check(
            "overlap",
            !event.overlap.unwrap_or(false),
            format!("recorded={:?}; required=false", event.overlap),
        ),
        explain_check(
            "degraded",
            !event.degraded.unwrap_or(false),
            format!("recorded={:?}; required=false", event.degraded),
        ),
        explain_check(
            "after_incomplete",
            !event.after_incomplete.unwrap_or(false),
            format!("recorded={:?}; required=false", event.after_incomplete),
        ),
        explain_check(
            "evidence_exactness",
            evidence == "exact",
            format!("event_evidence={evidence}; required=exact"),
        ),
    ];
    EventDecisionTrace {
        event: event.event.clone(),
        tool_use_id: event.tool_use_id.clone(),
        operation: event.operation.clone(),
        checks,
    }
}

fn commit_split_lines(bytes: &[u8]) -> Vec<Vec<u8>> {
    if bytes.is_empty() {
        return Vec::new();
    }
    let mut lines = bytes
        .split(|byte| *byte == b'\n')
        .map(<[u8]>::to_vec)
        .collect::<Vec<_>>();
    if bytes.last() == Some(&b'\n') {
        lines.pop();
    }
    lines
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

fn attribution_ranges(
    path: &str,
    lines: &[derive::AttributedLine],
    steps: &[Step],
) -> Vec<AttributionRange> {
    let mut ranges = Vec::new();
    for attributed in lines
        .iter()
        .filter(|line| line.line.added && !line.line.blank)
    {
        let step = attributed
            .source_step
            .and_then(|index| steps.get(index))
            .and_then(|step| step.attribution.as_ref());
        let label = attributed.line.label;
        let category = category_name(label);
        let mut limiting_basis = if label == derive::Label::Unknown {
            Vec::new()
        } else {
            step.map(|step| step.limiting_basis.clone())
                .unwrap_or_default()
        };
        if attributed.ambiguous {
            push_basis(&mut limiting_basis, "CONFLICTING_EVIDENCE");
        }
        if label == derive::Label::Mixed
            && !matches!(
                attributed.unobserved_edit,
                Some(
                    derive::UnobservedEditKind::MixedByTokenOverlap
                        | derive::UnobservedEditKind::MixedBySimilarity
                )
            )
        {
            push_basis(&mut limiting_basis, "CONFLICTING_EVIDENCE");
        }
        match attributed.unobserved_edit {
            Some(derive::UnobservedEditKind::ExactSurvivorInChangedFile) => {
                push_basis(&mut limiting_basis, "X_UNOBSERVED_EDIT_IN_FILE");
            }
            Some(derive::UnobservedEditKind::MixedByTokenOverlap)
            | Some(derive::UnobservedEditKind::UnmatchedUnknown) => {
                push_basis(&mut limiting_basis, "X_UNOBSERVED_EDIT");
            }
            Some(derive::UnobservedEditKind::MixedBySimilarity) => {
                push_basis(&mut limiting_basis, "X_UNOBSERVED_EDIT_IN_FILE");
            }
            None => {}
        }
        let score_step = step.cloned();
        let score = score_for_label(label, score_step.as_ref(), &limiting_basis);
        let confidence_band = match label {
            derive::Label::Unknown => "none",
            derive::Label::AiDirect
            | derive::Label::AiStrong
            | derive::Label::AiProbable
            | derive::Label::Mixed => score.map(confidence_band).unwrap_or("none"),
        };
        let attribution = AttributionRange {
            path: path.into(),
            start_line: attributed.line.line,
            end_line: attributed.line.line,
            end_line_exclusive: Some(attributed.line.line + 1),
            category: category.into(),
            score,
            confidence_band: confidence_band.into(),
            primary_basis: if label == derive::Label::Unknown {
                None
            } else if attributed.unobserved_edit
                == Some(derive::UnobservedEditKind::MixedByTokenOverlap)
                || attributed.unobserved_edit == Some(derive::UnobservedEditKind::MixedBySimilarity)
            {
                Some("CONFLICTING_EVIDENCE".into())
            } else {
                step.and_then(|step| step.primary_basis.clone())
            },
            supporting_basis: if label == derive::Label::Unknown {
                Vec::new()
            } else if attributed.unobserved_edit
                == Some(derive::UnobservedEditKind::MixedByTokenOverlap)
            {
                vec!["EXACT_FRAGMENT_MATCH".into()]
            } else {
                step.map(|step| step.supporting_basis.clone())
                    .unwrap_or_default()
            },
            limiting_basis,
            source: step.and_then(|step| {
                (step.adapter_id.is_some() || step.tool_id.is_some()).then(|| AttributionSource {
                    adapter_id: step.adapter_id.clone(),
                    tool_id: step.tool_id.clone(),
                    session_ref: step.run_id.clone(),
                })
            }),
            operation: step.and_then(|step| step.operation.clone()),
            algorithm_profile: "deterministic-v1".into(),
            score_profile: "score-v1".into(),
            exclusion_classifier: None,
            exclusion_reason: None,
        };
        push_attribution_range(&mut ranges, attribution);
    }
    ranges
}

fn prepare_unobserved_survival_steps(steps: &[Step]) -> Option<Vec<Step>> {
    let last_ai = steps
        .iter()
        .rposition(|step| matches!(step.kind, StepKind::Ai { .. }))?;
    if steps
        .iter()
        .any(|step| !matches!(step.kind, StepKind::Ai { .. }))
    {
        return None;
    }
    let last_attribution = steps[last_ai].attribution.as_ref()?;
    if last_attribution
        .limiting_basis
        .iter()
        .any(|basis| matches!(basis.as_str(), "X_DEGRADED" | "X_OVERLAPPING_SESSIONS"))
        || last_attribution.primary_basis.as_deref() == Some("INCOMPLETE_SOURCE")
    {
        return None;
    }

    let mut prepared = steps.to_vec();
    for step in &mut prepared {
        let StepKind::Ai { degraded: true } = step.kind else {
            continue;
        };
        let Some(attribution) = step.attribution.as_mut() else {
            continue;
        };
        if !attribution
            .limiting_basis
            .iter()
            .any(|basis| basis == "X_TIME_CUT")
            || attribution
                .limiting_basis
                .iter()
                .any(|basis| matches!(basis.as_str(), "X_DEGRADED" | "X_OVERLAPPING_SESSIONS"))
        {
            continue;
        }
        attribution
            .limiting_basis
            .retain(|basis| basis != "X_TIME_CUT");
        if let Some(penalty) = attribution
            .penalties_milli
            .iter()
            .position(|value| *value == 120)
        {
            attribution.penalties_milli.remove(penalty);
        }
        step.kind = StepKind::Ai { degraded: false };
    }
    Some(prepared)
}

fn missing_snapshot_time_cut_fallback(
    store: &Path,
    events: &[&EventRecord],
    committed_raw: &[u8],
    committed_normalized: &[u8],
) -> Option<Step> {
    let committed_sha = crate::hash(committed_raw);
    let event = events.iter().find(|event| {
        if event.event != "ai_change" {
            return false;
        }
        let Some(sha) = event.after_sha.as_deref() else {
            return false;
        };
        is_sha256(sha)
            && sha != committed_sha
            && crate::read_blob(&store.join("blobs").join(sha), sha).is_none()
    })?;
    Some(Step {
        kind: StepKind::Ai { degraded: true },
        after: committed_normalized.to_vec(),
        attribution: Some(derive::StepAttribution {
            adapter_id: event.tool.clone(),
            tool_id: event.tool.clone(),
            run_id: event.tool_use_id.clone(),
            session_id: event.session_id.clone(),
            operation: event.operation.clone(),
            primary_basis: Some("INCOMPLETE_SOURCE".into()),
            score_milli: 920,
            penalties_milli: vec![120],
            supporting_basis: Vec::new(),
            limiting_basis: vec!["X_TIME_CUT".into(), "INCOMPLETE_SOURCE".into()],
            direct_lines: Vec::new(),
        }),
    })
}

fn read_parent_line_labels(
    store: &Path,
    parent: &str,
) -> HashMap<String, HashMap<usize, StoredParentLine>> {
    let path = store.join("commits").join(format!("{parent}.lines.json"));
    let Ok(bytes) = fs::read(path) else {
        return HashMap::new();
    };
    let Ok(document) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return HashMap::new();
    };
    let mut files = HashMap::<String, HashMap<usize, StoredParentLine>>::new();
    if let Some(entries) = document.get("files").and_then(serde_json::Value::as_array) {
        for file in entries {
            let Some(path) = file.get("path").and_then(serde_json::Value::as_str) else {
                continue;
            };
            let Some(lines) = file.get("lines").and_then(serde_json::Value::as_array) else {
                continue;
            };
            let line_map = files.entry(path.to_owned()).or_default();
            for line in lines {
                let (Some(number), Some(code)) = (
                    line.get("line").and_then(serde_json::Value::as_u64),
                    line.get("label").and_then(serde_json::Value::as_str),
                ) else {
                    continue;
                };
                let Some(category) = category_for_line_code(code) else {
                    continue;
                };
                line_map.entry(number as usize).or_default().category = category.into();
            }
        }
    }
    if let Some(ranges) = document
        .get("attributions")
        .and_then(serde_json::Value::as_array)
    {
        for range in ranges {
            let (Some(path), Some(start), Some(end), Some(category)) = (
                range.get("path").and_then(serde_json::Value::as_str),
                range.get("startLine").and_then(serde_json::Value::as_u64),
                range.get("endLine").and_then(serde_json::Value::as_u64),
                range.get("category").and_then(serde_json::Value::as_str),
            ) else {
                continue;
            };
            let source = range
                .get("source")
                .and_then(serde_json::Value::as_object)
                .map(|source| AttributionSource {
                    adapter_id: source
                        .get("adapterId")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned),
                    tool_id: source
                        .get("toolId")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned),
                    session_ref: source
                        .get("sessionRef")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned),
                });
            let supporting_basis: Vec<String> = range
                .get("supportingBasis")
                .and_then(serde_json::Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(serde_json::Value::as_str)
                .map(str::to_owned)
                .collect();
            let score = range.get("score").and_then(serde_json::Value::as_f64);
            let operation = range
                .get("operation")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned);
            let exclusion_classifier = range
                .get("exclusionClassifier")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned);
            let exclusion_reason = range
                .get("exclusionReason")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned);
            let line_map = files.entry(path.to_owned()).or_default();
            for line in start..=end {
                let stored = line_map.entry(line as usize).or_default();
                stored.category = category.to_owned();
                stored.score = score;
                stored.source = source.clone();
                stored.operation = operation.clone();
                stored.supporting_basis = supporting_basis.clone();
                stored.exclusion_classifier = exclusion_classifier.clone();
                stored.exclusion_reason = exclusion_reason.clone();
            }
        }
    }
    files
}

fn read_latest_path_line_labels(
    root: &Path,
    store: &Path,
    parent: &str,
    path: &str,
) -> Option<HashMap<usize, StoredParentLine>> {
    let commit = git_checked(root, &["log", "-1", "--format=%H", parent, "--", path]).ok()?;
    let commit = commit.trim();
    if commit.is_empty() {
        return None;
    }
    let mut labels = read_parent_line_labels(store, commit);
    labels.remove(path)
}

fn category_for_line_code(code: &str) -> Option<&'static str> {
    match code {
        "d" => Some("ai_direct"),
        "s" => Some("ai_strong"),
        "p" => Some("ai_probable"),
        "m" => Some("mixed"),
        "u" => Some("unknown"),
        _ => None,
    }
}

fn default_parent_score_milli(category: &str) -> u16 {
    match category {
        "ai_direct" => 990,
        "ai_strong" => 920,
        "ai_probable" => 790,
        "mixed" => MIXED_SCORE_MILLI,
        _ => 0,
    }
}

fn parent_lineage_fallback_matches(
    parent: &[u8],
    committed: &[u8],
    stored_lines: &HashMap<usize, StoredParentLine>,
) -> Vec<(usize, StoredParentLine)> {
    let parent_lines = split_normalized_lines(parent);
    let committed_lines = split_normalized_lines(committed);
    let operations = capture_diff_slices(Algorithm::Myers, &parent_lines, &committed_lines);
    let mut matches = Vec::new();
    let mut cursor = 0;
    while cursor < operations.len() {
        if operations[cursor].tag() == DiffTag::Equal {
            cursor += 1;
            continue;
        }
        let hunk_start = cursor;
        while cursor < operations.len() && operations[cursor].tag() != DiffTag::Equal {
            cursor += 1;
        }
        let removed_ai: Vec<_> = operations[hunk_start..cursor]
            .iter()
            .flat_map(|operation| operation.old_range())
            .filter_map(|line_index| {
                stored_lines
                    .get(&(line_index + 1))
                    .filter(|line| line.is_ai())
                    .map(|line| (line_index, line))
            })
            .collect();
        let added: Vec<_> = operations[hunk_start..cursor]
            .iter()
            .flat_map(|operation| operation.new_range())
            .filter(|&line_index| {
                !committed_lines[line_index]
                    .iter()
                    .all(u8::is_ascii_whitespace)
            })
            .collect();
        if removed_ai.is_empty() || added.is_empty() {
            continue;
        }
        let old_text: Vec<_> = removed_ai
            .iter()
            .map(|(line_index, _)| parent_lines[*line_index])
            .collect();
        let new_text: Vec<_> = added
            .iter()
            .map(|&line_index| committed_lines[line_index])
            .collect();
        for (old_match, new_match) in order_preserving_similarity_matches(&old_text, &new_text) {
            matches.push((added[new_match], removed_ai[old_match].1.clone()));
        }
    }
    matches
}

#[derive(Clone, Copy, Default)]
struct MatchQuality {
    pairs: usize,
    similarity: u64,
}

fn order_preserving_similarity_matches(old: &[&[u8]], new: &[&[u8]]) -> Vec<(usize, usize)> {
    if old.is_empty()
        || new.is_empty()
        || old.len().saturating_mul(new.len()) > MAX_PARENT_MATCH_CELLS
    {
        return Vec::new();
    }
    const SKIP_OLD: u8 = 1;
    const SKIP_NEW: u8 = 2;
    const PAIR: u8 = 3;
    let width = new.len() + 1;
    let mut choices = vec![0u8; (old.len() + 1) * width];
    let mut next = vec![MatchQuality::default(); width];
    for old_index in (0..old.len()).rev() {
        let mut current = vec![MatchQuality::default(); width];
        for new_index in (0..new.len()).rev() {
            let mut best = next[new_index];
            let mut choice = SKIP_OLD;
            if better_match_quality(current[new_index + 1], best) {
                best = current[new_index + 1];
                choice = SKIP_NEW;
            }
            let similarity = normalized_line_similarity(old[old_index], new[new_index]);
            if similarity >= MIN_PARENT_EDIT_SIMILARITY {
                let mut paired = next[new_index + 1];
                paired.pairs += 1;
                paired.similarity += (similarity * 1_000_000.0).round() as u64;
                if better_match_quality(paired, best) {
                    best = paired;
                    choice = PAIR;
                }
            }
            current[new_index] = best;
            choices[old_index * width + new_index] = choice;
        }
        next = current;
    }
    let mut pairs = Vec::new();
    let (mut old_index, mut new_index) = (0, 0);
    while old_index < old.len() && new_index < new.len() {
        match choices[old_index * width + new_index] {
            PAIR => {
                pairs.push((old_index, new_index));
                old_index += 1;
                new_index += 1;
            }
            SKIP_NEW => new_index += 1,
            _ => old_index += 1,
        }
    }
    pairs
}

fn better_match_quality(candidate: MatchQuality, current: MatchQuality) -> bool {
    candidate.pairs > current.pairs
        || (candidate.pairs == current.pairs && candidate.similarity > current.similarity)
}

/// Similarity is 1 - normalized bytewise Levenshtein distance; pairs below 0.5 stay unknown.
fn normalized_line_similarity(left: &[u8], right: &[u8]) -> f64 {
    let denominator = left.len().max(right.len());
    if denominator == 0 {
        return 1.0;
    }
    let distance = levenshtein_distance(left, right);
    1.0 - distance as f64 / denominator as f64
}

fn levenshtein_distance(left: &[u8], right: &[u8]) -> usize {
    let mut previous: Vec<_> = (0..=right.len()).collect();
    let mut current = vec![0; right.len() + 1];
    for (left_index, left_byte) in left.iter().enumerate() {
        current[0] = left_index + 1;
        for (right_index, right_byte) in right.iter().enumerate() {
            current[right_index + 1] = (previous[right_index + 1] + 1)
                .min(current[right_index] + 1)
                .min(previous[right_index] + usize::from(left_byte != right_byte));
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[right.len()]
}

fn unknown_attributions(path: &str, parent: &[u8], committed: &[u8]) -> Vec<AttributionRange> {
    let evidence = Evidence {
        parent: parent.to_vec(),
        steps: Vec::new(),
    };
    attribution_ranges(
        path,
        &derive::derive_lines_with_attribution(&evidence, committed),
        &[],
    )
}

fn excluded_attributions(
    path: &str,
    parent: &[u8],
    committed: &[u8],
    reason: &str,
    limiting: Option<&str>,
) -> Vec<AttributionRange> {
    let lines = derive::derive_lines(
        &Evidence {
            parent: parent.to_vec(),
            steps: Vec::new(),
        },
        committed,
    );
    let mut ranges = Vec::new();
    for line in lines.iter().filter(|line| line.added && !line.blank) {
        push_attribution_range(
            &mut ranges,
            AttributionRange {
                path: path.into(),
                start_line: line.line,
                end_line: line.line,
                end_line_exclusive: Some(line.line + 1),
                category: "excluded".into(),
                score: None,
                confidence_band: "none".into(),
                primary_basis: None,
                supporting_basis: Vec::new(),
                limiting_basis: limiting.into_iter().map(str::to_owned).collect(),
                source: None,
                operation: None,
                algorithm_profile: "deterministic-v1".into(),
                score_profile: "score-v1".into(),
                exclusion_classifier: Some(crate::exclusion::CLASSIFIER_ID.into()),
                exclusion_reason: Some(reason.into()),
            },
        );
    }
    ranges
}

fn rename_line_attributions(
    path: &str,
    parent: &[u8],
    committed: &[u8],
    stored_lines: &HashMap<usize, StoredParentLine>,
) -> Vec<AttributionRange> {
    let parent_lines = split_normalized_lines(parent);
    let committed_lines = split_normalized_lines(committed);
    let operations = capture_diff_slices(Algorithm::Myers, &parent_lines, &committed_lines);
    let mut ranges = Vec::new();
    for operation in operations
        .iter()
        .filter(|operation| operation.tag() == DiffTag::Equal)
    {
        for (old_line, new_line) in operation.old_range().zip(operation.new_range()) {
            let Some(stored) = stored_lines.get(&(old_line + 1)) else {
                continue;
            };
            if stored.category.is_empty() {
                continue;
            }
            let line = new_line + 1;
            let confidence_band = stored.score.map(confidence_band).unwrap_or("none");
            ranges.push(AttributionRange {
                path: path.into(),
                start_line: line,
                end_line: line,
                end_line_exclusive: Some(line + 1),
                category: stored.category.clone(),
                score: stored.score,
                confidence_band: confidence_band.into(),
                primary_basis: None,
                supporting_basis: stored.supporting_basis.clone(),
                limiting_basis: Vec::new(),
                source: stored.source.clone(),
                operation: stored.operation.clone(),
                algorithm_profile: "deterministic-v1".into(),
                score_profile: "score-v1".into(),
                exclusion_classifier: stored.exclusion_classifier.clone(),
                exclusion_reason: stored.exclusion_reason.clone(),
            });
        }
    }
    ranges
}

fn category_name(label: derive::Label) -> &'static str {
    match label {
        derive::Label::AiDirect => "ai_direct",
        derive::Label::AiStrong => "ai_strong",
        derive::Label::AiProbable => "ai_probable",
        derive::Label::Mixed => "mixed",
        derive::Label::Unknown => "unknown",
    }
}

fn score_for_label(
    label: derive::Label,
    step: Option<&derive::StepAttribution>,
    limiting_basis: &[String],
) -> Option<f64> {
    if matches!(label, derive::Label::Unknown) {
        return None;
    }
    if matches!(label, derive::Label::Mixed) {
        return Some(f64::from(MIXED_SCORE_MILLI) / 1000.0);
    }
    let mut score = i32::from(step.map_or(0, |step| step.score_milli));
    score -= step
        .map(|step| {
            step.penalties_milli
                .iter()
                .copied()
                .map(i32::from)
                .sum::<i32>()
        })
        .unwrap_or_default();
    if limiting_basis
        .iter()
        .any(|entry| entry == "CONFLICTING_EVIDENCE")
    {
        score -= 80;
    }
    score = match label {
        derive::Label::AiDirect => score.clamp(980, 1000),
        derive::Label::AiStrong => score.clamp(800, 970),
        derive::Label::AiProbable => score.clamp(500, 790),
        derive::Label::Mixed => score.clamp(0, 1000),
        derive::Label::Unknown => return None,
    };
    Some(f64::from(score) / 1000.0)
}

pub(crate) fn confidence_band(score: f64) -> &'static str {
    if score >= 0.80 {
        "high"
    } else if score >= 0.50 {
        "medium"
    } else {
        "low"
    }
}

fn push_basis(basis: &mut Vec<String>, value: &str) {
    if !basis.iter().any(|entry| entry == value) {
        basis.push(value.into());
    }
}

fn push_attribution_range(ranges: &mut Vec<AttributionRange>, next: AttributionRange) {
    if let Some(previous) = ranges.last_mut() {
        if previous.path == next.path
            && previous.end_line + 1 == next.start_line
            && previous.category == next.category
            && previous.score == next.score
            && previous.confidence_band == next.confidence_band
            && previous.primary_basis == next.primary_basis
            && previous.supporting_basis == next.supporting_basis
            && previous.limiting_basis == next.limiting_basis
            && previous.source == next.source
            && previous.operation == next.operation
            && previous.algorithm_profile == next.algorithm_profile
            && previous.score_profile == next.score_profile
            && previous.exclusion_classifier == next.exclusion_classifier
            && previous.exclusion_reason == next.exclusion_reason
        {
            previous.end_line = next.end_line;
            previous.end_line_exclusive = next.end_line_exclusive;
            return;
        }
    }
    ranges.push(next);
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
        ai_direct: 0,
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
        ai_direct: 0,
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
        ai_direct: 0,
        ai_strong: 0,
        ai_probable: 0,
        mixed: 0,
        unknown: added,
        excluded: 0,
        evidence,
        carried: None,
    }
}

pub fn percentage_tenths(numerator: usize, denominator: usize) -> Option<f64> {
    if denominator == 0 {
        None
    } else {
        let tenths =
            ((numerator as u128 * 1000 + denominator as u128 / 2) / denominator as u128) as u64;
        Some(tenths as f64 / 10.0)
    }
}

pub fn coverage_band(covered: usize, total: usize) -> &'static str {
    if total == 0 {
        "unknown"
    } else if covered.saturating_mul(3) < total {
        "low"
    } else if covered.saturating_mul(3) < total.saturating_mul(2) {
        "partial"
    } else {
        "high"
    }
}

pub fn coverage_band_with_unobserved(
    covered: usize,
    total: usize,
    unobserved_lines: usize,
) -> &'static str {
    coverage_band_limited(coverage_band(covered, total), total, unobserved_lines)
}

pub fn coverage_band_limited(band: &str, total: usize, unobserved_lines: usize) -> &'static str {
    if total == 0 {
        return match band {
            "high" => "high",
            "medium" | "partial" => "partial",
            "low" => "low",
            "complete" => "complete",
            "unknown" => "unknown",
            "none" => "unknown",
            _ => "unknown",
        };
    }
    let band = match band {
        "medium" => "partial",
        "none" => "low",
        other => other,
    };
    let unobserved_percent = (unobserved_lines as u128).saturating_mul(100);
    let total = total as u128;
    if unobserved_percent >= total.saturating_mul(UNOBSERVED_COVERAGE_LOW_PERCENT as u128) {
        "low"
    } else if unobserved_percent >= total.saturating_mul(UNOBSERVED_COVERAGE_MEDIUM_PERCENT as u128)
    {
        if band == "high" {
            "partial"
        } else {
            match band {
                "partial" => "partial",
                "low" => "low",
                "complete" => "complete",
                "unknown" => "unknown",
                _ => "unknown",
            }
        }
    } else {
        match band {
            "high" => "high",
            "partial" => "partial",
            "low" => "low",
            "complete" => "complete",
            "unknown" => "unknown",
            _ => "unknown",
        }
    }
}

pub fn unobserved_eligible_lines_from_sidecar(bytes: &[u8]) -> usize {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(bytes) else {
        return 0;
    };
    let Ok(attributions) =
        serde_json::from_value::<Vec<AttributionRange>>(value["attributions"].clone())
    else {
        return 0;
    };
    unobserved_eligible_line_count(&attributions)
}

fn unobserved_eligible_line_count(attributions: &[AttributionRange]) -> usize {
    let mut lines = BTreeSet::new();
    for range in attributions {
        if range.category == "excluded"
            || !range
                .limiting_basis
                .iter()
                .any(|basis| UNOBSERVED_COVERAGE_BASIS.contains(&basis.as_str()))
        {
            continue;
        }
        lines.extend((range.start_line..=range.end_line).map(|line| (range.path.as_str(), line)));
    }
    lines.len()
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

fn git_checked(root: &Path, args: &[&str]) -> Result<String, String> {
    let output = git_output(root, args)?;
    if !output.status.success() {
        return Err(format!("git {} failed", args.join(" ")));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn test_store(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("prov-commit-{name}-{}-{nonce}", std::process::id()))
    }

    fn put_blob(store: &Path, bytes: &[u8]) -> String {
        let digest = crate::hash(bytes);
        let directory = store.join("blobs");
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join(&digest), bytes).unwrap();
        digest
    }

    #[test]
    fn unobserved_edit_coverage_thresholds_and_r22_fixtures() {
        assert_eq!(coverage_band_with_unobserved(100, 100, 0), "high");
        assert_eq!(coverage_band_with_unobserved(100, 100, 4), "high");
        assert_eq!(coverage_band_with_unobserved(100, 100, 5), "partial");
        assert_eq!(coverage_band_with_unobserved(100, 100, 19), "partial");
        assert_eq!(coverage_band_with_unobserved(100, 100, 20), "low");
        assert_eq!(coverage_band(0, 0), "unknown");
        assert_eq!(coverage_band_limited("medium", 100, 0), "partial");
        assert_eq!(coverage_band_limited("none", 0, 0), "unknown");
        assert_eq!(coverage_band_limited("none", 10, 0), "low");

        let shell_lines = fixture_unobserved_line_count(include_str!(
            "../tests/fixtures/r22_coverage/repos/82ecea8ad5090c0735a1419ee1e73701ad9a110aee0aa756ec6cf6b87ceeb3d8/commits/22e8fb2184e9397f134648d58f14f722e0dce654.lines.json"
        ));
        let direct_lines = fixture_unobserved_line_count(include_str!(
            "../tests/fixtures/r22_coverage/repos/82ecea8ad5090c0735a1419ee1e73701ad9a110aee0aa756ec6cf6b87ceeb3d8/commits/d11cbaaad40c74ebf7308f214914d7c51f4b8823.lines.json"
        ));
        let edited_lines = fixture_unobserved_line_count(include_str!(
            "../tests/fixtures/r22_coverage/repos/82ecea8ad5090c0735a1419ee1e73701ad9a110aee0aa756ec6cf6b87ceeb3d8/commits/b1a6fd424d42edad66bcc2ca181c84acac6021c0.lines.json"
        ));
        assert_eq!(shell_lines, 0);
        assert_eq!(direct_lines, 0);
        assert_eq!(edited_lines, 38);
        assert_eq!(coverage_band_with_unobserved(9, 9, shell_lines), "high");
        assert_eq!(coverage_band_with_unobserved(86, 86, direct_lines), "high");
        assert_eq!(coverage_band_with_unobserved(163, 163, edited_lines), "low");
    }

    fn fixture_unobserved_line_count(sidecar: &str) -> usize {
        let value: serde_json::Value = serde_json::from_str(sidecar).unwrap();
        let attributions: Vec<AttributionRange> =
            serde_json::from_value(value["attributions"].clone()).unwrap();
        unobserved_eligible_line_count(&attributions)
    }

    fn event(
        after_sha: Option<String>,
        event: &str,
        degraded: bool,
        reason: Option<&str>,
    ) -> EventRecord {
        EventRecord {
            event: event.into(),
            timestamp: 10,
            path: Some("file.txt".into()),
            tool: Some("codex".into()),
            tool_use_id: Some("tool-1".into()),
            session_id: Some("session-1".into()),
            window_open: None,
            window_close: None,
            operation: Some("apply_patch".into()),
            structured_edit: None,
            line_ranges: None,
            before_sha: None,
            after_sha,
            degraded: Some(degraded),
            overlap: Some(false),
            after_incomplete: Some(false),
            reason: reason.map(str::to_owned),
            new_head: None,
            commit_in_window: None,
            paths: None,
            truncated: None,
            order: 0,
        }
    }

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
    fn event_overlap_requires_two_closed_tool_windows() {
        let fixture =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../prov-hook/tests/fixtures/real_overlap");
        let events = read_events(&fixture);
        let codex_edits: Vec<_> = events
            .iter()
            .filter(|event| event.event == "ai_change")
            .collect();
        assert!(!codex_edits.is_empty());
        assert!(codex_edits.iter().all(|event| event.overlap == Some(false)));
        assert!(events.iter().any(|event| event.event == "turn_end"
            && event.session_id.as_deref() == Some("0754415b-0754-4db2-8c70-82536857c41c")));

        let mut concurrent = Vec::new();
        let mut first_open = event(None, "window_open", false, None);
        first_open.session_id = Some("first".into());
        first_open.tool_use_id = Some("first-tool".into());
        first_open.window_open = Some(10);
        concurrent.push(first_open);
        let mut second_open = event(None, "window_open", false, None);
        second_open.session_id = Some("second".into());
        second_open.tool_use_id = Some("second-tool".into());
        second_open.window_open = Some(20);
        concurrent.push(second_open);
        let mut first_close = event(None, "window_close", false, None);
        first_close.session_id = Some("first".into());
        first_close.tool_use_id = Some("first-tool".into());
        first_close.window_close = Some(30);
        concurrent.push(first_close);
        let mut second_close = event(None, "window_close", false, None);
        second_close.session_id = Some("second".into());
        second_close.tool_use_id = Some("second-tool".into());
        second_close.window_close = Some(40);
        concurrent.push(second_close);
        let mut edit = event(Some("after".into()), "ai_change", false, None);
        edit.session_id = Some("first".into());
        edit.tool_use_id = Some("first-tool".into());
        concurrent.push(edit);
        normalize_window_overlaps(&mut concurrent);
        assert_eq!(concurrent[4].overlap, Some(true));
    }

    #[test]
    fn patch_id_diff_size_limit_includes_eight_mib_and_rejects_larger() {
        assert!(patch_diff_size_allowed(MAX_PATCH_DIFF_SIZE));
        assert!(!patch_diff_size_allowed(MAX_PATCH_DIFF_SIZE + 1));
    }

    #[test]
    fn current_event_evidence_modes_map_to_the_existing_steps() {
        let store = test_store("evidence-modes");
        let exact_bytes = b"base\nai\n";
        let exact_sha = put_blob(&store, exact_bytes);
        let exact = event(Some(exact_sha.clone()), "ai_change", false, None);
        let (steps, evidence) = steps_for_events(&store, &[&exact], b"", exact_bytes, 20);
        assert_eq!(evidence, "exact");
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].kind, StepKind::Ai { degraded: false });

        let (steps, evidence) = steps_for_events(&store, &[&exact], b"", b"base\nai\nlate\n", 20);
        assert_eq!(evidence, "time_cut");
        assert_eq!(steps[0].kind, StepKind::Ai { degraded: true });

        let (steps, evidence) = steps_for_events(&store, &[], b"", b"base\n", 20);
        assert_eq!(evidence, "none");
        assert!(steps.is_empty());

        let missing = event(Some("a".repeat(64)), "ai_change", false, None);
        let (steps, evidence) = steps_for_events(&store, &[&missing], b"", b"different\n", 20);
        assert_eq!(evidence, "missing_blob");
        assert!(steps.is_empty());

        let unavailable = event(
            Some(exact_sha),
            "external_change",
            true,
            Some("before_unavailable"),
        );
        let (steps, evidence) = steps_for_events(&store, &[&unavailable], b"", exact_bytes, 20);
        assert_eq!(evidence, "exact");
        assert_eq!(steps[0].kind, StepKind::Unknown);
        let _ = fs::remove_dir_all(store);
    }

    #[test]
    fn score_v1_applies_listed_penalties_and_category_caps() {
        let exact = derive::StepAttribution {
            adapter_id: Some("codex".into()),
            tool_id: Some("tool-1".into()),
            run_id: None,
            session_id: None,
            operation: Some("Bash".into()),
            primary_basis: Some("EXACT_FRAGMENT_MATCH".into()),
            score_milli: 920,
            penalties_milli: Vec::new(),
            supporting_basis: Vec::new(),
            limiting_basis: Vec::new(),
            direct_lines: Vec::new(),
        };
        assert_eq!(
            score_for_label(derive::Label::AiStrong, Some(&exact), &[]),
            Some(0.92)
        );
        assert_eq!(
            score_for_label(derive::Label::Mixed, Some(&exact), &[]),
            Some(0.84)
        );

        let missing_lineage = derive::StepAttribution {
            penalties_milli: vec![80],
            ..exact.clone()
        };
        assert_eq!(
            score_for_label(derive::Label::AiStrong, Some(&missing_lineage), &[]),
            Some(0.84)
        );

        let timestamp_only = derive::StepAttribution {
            penalties_milli: vec![120],
            limiting_basis: vec!["X_TIME_CUT".into()],
            ..exact.clone()
        };
        assert_eq!(
            score_for_label(derive::Label::AiProbable, Some(&timestamp_only), &[]),
            Some(0.79)
        );

        let incomplete = derive::StepAttribution {
            penalties_milli: vec![100],
            limiting_basis: vec!["INCOMPLETE_SOURCE".into()],
            ..exact.clone()
        };
        assert_eq!(
            score_for_label(derive::Label::AiProbable, Some(&incomplete), &[]),
            Some(0.79)
        );

        assert_eq!(
            score_for_label(
                derive::Label::AiProbable,
                Some(&exact),
                &["CONFLICTING_EVIDENCE".into()],
            ),
            Some(0.79)
        );
        assert_eq!(
            score_for_label(
                derive::Label::AiProbable,
                Some(&derive::StepAttribution {
                    primary_basis: Some("MOVE_LINEAGE".into()),
                    score_milli: 600,
                    ..exact
                }),
                &["X_RECONSTRUCTED_WINDOW".into()],
            ),
            Some(0.6)
        );
    }
}
