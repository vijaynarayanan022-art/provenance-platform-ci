pub mod commit;
pub mod derive;
pub mod exclusion;
pub mod rewrite;

use fd_lock::RwLock;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use similar::{ChangeTag, TextDiff};
use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub fn configure_process_command(command: &mut Command, background_derive: bool) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(windows_process_creation_flags(background_derive));
    }

    #[cfg(not(windows))]
    let _ = (command, background_derive);
}

#[cfg(windows)]
fn windows_process_creation_flags(background_derive: bool) -> u32 {
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    CREATE_NO_WINDOW
        | if background_derive {
            CREATE_NEW_PROCESS_GROUP
        } else {
            0
        }
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct TimingSnapshot {
    pub total_ns: u64,
    pub git_processes: u64,
    pub repo_discovery_ns: u64,
    pub lock_ns: u64,
    pub git_status_ns: u64,
    pub hashing_snapshots_ns: u64,
    pub writes_ns: u64,
    pub sync_ns: u64,
    pub log_work_ns: u64,
}
struct TimingAtoms {
    total_ns: AtomicU64,
    git_processes: AtomicU64,
    repo_discovery_ns: AtomicU64,
    lock_ns: AtomicU64,
    git_status_ns: AtomicU64,
    hashing_snapshots_ns: AtomicU64,
    writes_ns: AtomicU64,
    sync_ns: AtomicU64,
    log_work_ns: AtomicU64,
}
impl TimingAtoms {
    const fn new() -> Self {
        Self {
            total_ns: AtomicU64::new(0),
            git_processes: AtomicU64::new(0),
            repo_discovery_ns: AtomicU64::new(0),
            lock_ns: AtomicU64::new(0),
            git_status_ns: AtomicU64::new(0),
            hashing_snapshots_ns: AtomicU64::new(0),
            writes_ns: AtomicU64::new(0),
            sync_ns: AtomicU64::new(0),
            log_work_ns: AtomicU64::new(0),
        }
    }
    fn reset(&self) {
        for a in [
            &self.total_ns,
            &self.git_processes,
            &self.repo_discovery_ns,
            &self.lock_ns,
            &self.git_status_ns,
            &self.hashing_snapshots_ns,
            &self.writes_ns,
            &self.sync_ns,
            &self.log_work_ns,
        ] {
            a.store(0, Ordering::Relaxed);
        }
    }
    fn snapshot(&self) -> TimingSnapshot {
        TimingSnapshot {
            total_ns: self.total_ns.load(Ordering::Relaxed),
            git_processes: self.git_processes.load(Ordering::Relaxed),
            repo_discovery_ns: self.repo_discovery_ns.load(Ordering::Relaxed),
            lock_ns: self.lock_ns.load(Ordering::Relaxed),
            git_status_ns: self.git_status_ns.load(Ordering::Relaxed),
            hashing_snapshots_ns: self.hashing_snapshots_ns.load(Ordering::Relaxed),
            writes_ns: self.writes_ns.load(Ordering::Relaxed),
            sync_ns: self.sync_ns.load(Ordering::Relaxed),
            log_work_ns: self.log_work_ns.load(Ordering::Relaxed),
        }
    }
}
static TIMINGS: TimingAtoms = TimingAtoms::new();
fn add_ns(target: &AtomicU64, duration: Duration) {
    target.fetch_add(
        duration.as_nanos().min(u64::MAX as u128) as u64,
        Ordering::Relaxed,
    );
}

const MAX_FILE: u64 = 1024 * 1024;
const SCAN_BUDGET: Duration = Duration::from_millis(1500);
const LOCK_BUDGET: Duration = Duration::from_millis(500);
const MAX_DIRTY_PATHS: usize = 500;
const MAX_RECENT_KEYS: usize = 1000;
const MAX_WINDOW_AGE_NS: u128 = 60 * 60 * 1_000_000_000;
#[derive(Debug, Deserialize)]
struct HookInput {
    hook_event_name: Option<String>,
    session_id: Option<String>,
    turn_id: Option<String>,
    cwd: Option<String>,
    model: Option<String>,
    tool_name: Option<String>,
    tool_use_id: Option<String>,
    tool_input: Option<serde_json::Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct FileState {
    size: u64,
    mtime_ns: u128,
    sha256: String,
    #[serde(default)]
    excluded: bool,
    #[serde(default)]
    deleted: bool,
}
#[derive(Debug, Default, Serialize, Deserialize)]
struct Shadow {
    files: BTreeMap<String, FileState>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct WindowState {
    session_id: String,
    tool_use_id: Option<String>,
    #[serde(default)]
    target_paths: Option<Vec<String>>,
    opened_at: u128,
    head: String,
    branch: String,
    overlap: bool,
    #[serde(default)]
    bulk_count: Option<usize>,
    #[serde(default)]
    bulk_remaining: Option<usize>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct RecentKey {
    tool_use_id: String,
    path: String,
    after_sha: Option<String>,
    timestamp: u128,
}
#[derive(Debug, Default, Serialize, Deserialize)]
struct RepoState {
    windows: Vec<WindowState>,
    recent_keys: Vec<RecentKey>,
    scan_incomplete_pending: bool,
}
#[derive(Debug, Serialize, Deserialize)]
struct RepoCache {
    repo_id: String,
    #[serde(default)]
    root_check: String,
    #[serde(default)]
    git_dir_created_ns: Option<u128>,
}

#[derive(Debug, Serialize)]
struct Event {
    schema_version: &'static str,
    event: String,
    timestamp: u128,
    hook_event_name: Option<String>,
    repo_id: Option<String>,
    path: Option<String>,
    before_sha: Option<String>,
    after_sha: Option<String>,
    line_ranges: Option<Vec<[usize; 2]>>,
    deleted_at: Option<Vec<usize>>,
    session_id: Option<String>,
    turn_id: Option<String>,
    tool_use_id: Option<String>,
    model: Option<String>,
    tool: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    operation: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    structured_edit: Option<bool>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    structured_added_lines: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    structured_input_sha256: Option<String>,
    command_sha256: Option<String>,
    command_length: Option<usize>,
    window_open: Option<u128>,
    window_close: Option<u128>,
    overlap: Option<bool>,
    degraded: Option<bool>,
    reason: Option<String>,
    excluded: Option<bool>,
    after_incomplete: Option<bool>,
    head: Option<String>,
    branch: Option<String>,
    head_moved: Option<bool>,
    old_head: Option<String>,
    new_head: Option<String>,
    branch_before: Option<String>,
    branch_after: Option<String>,
    file_count: Option<usize>,
    paths: Option<Vec<String>>,
    truncated: Option<bool>,
    commit_in_window: Option<bool>,
    dirty_count: Option<usize>,
    remaining: Option<usize>,
    bulk_partial: Option<bool>,
}
impl Event {
    fn new(
        name: &str,
        hook: Option<&str>,
        repo: Option<&str>,
        session: Option<&str>,
        tool: Option<&str>,
    ) -> Self {
        Self {
            schema_version: "prov.event/0.1",
            event: name.into(),
            timestamp: now_ns(),
            hook_event_name: hook.map(str::to_owned),
            repo_id: repo.map(str::to_owned),
            path: None,
            before_sha: None,
            after_sha: None,
            line_ranges: None,
            deleted_at: None,
            session_id: session.map(str::to_owned),
            turn_id: None,
            tool_use_id: tool.map(str::to_owned),
            model: None,
            tool: None,
            operation: None,
            structured_edit: None,
            structured_added_lines: Vec::new(),
            structured_input_sha256: None,
            command_sha256: None,
            command_length: None,
            window_open: None,
            window_close: None,
            overlap: None,
            degraded: None,
            reason: None,
            excluded: None,
            after_incomplete: None,
            head: None,
            branch: None,
            head_moved: None,
            old_head: None,
            new_head: None,
            branch_before: None,
            branch_after: None,
            file_count: None,
            paths: None,
            truncated: None,
            commit_in_window: None,
            dirty_count: None,
            remaining: None,
            bulk_partial: None,
        }
    }
}

#[derive(Clone)]
struct MarkerContext {
    home: PathBuf,
    repo_id: Option<String>,
    session_id: Option<String>,
    tool_use_id: Option<String>,
    hook_event_name: Option<String>,
}
struct RepoInfo {
    root: PathBuf,
    git_dir: PathBuf,
    common_dir: PathBuf,
    repo_id: Option<String>,
    shallow: bool,
    head: String,
    branch: String,
}
#[derive(Clone, Debug)]
struct DirtyPath {
    path: String,
    head_blob: Option<String>,
    untracked: bool,
    unmerged: bool,
    renamed: bool,
}
#[derive(Clone, Debug, PartialEq, Eq)]
enum BeforeContent {
    Known(Vec<u8>),
    KnownEmpty,
    Unavailable,
}
#[derive(Clone, Debug)]
struct FileChange {
    path: String,
    before: Option<String>,
    after: Option<String>,
    excluded: bool,
    degraded: Option<String>,
    line_ranges: Option<Vec<[usize; 2]>>,
    deleted_at: Vec<usize>,
}

/// Contain all hook work. The executable wraps this in catch_unwind and has a process watchdog.
pub fn process_json(input: &[u8]) {
    let Ok(hook) = serde_json::from_slice::<HookInput>(input) else {
        return;
    };
    process_hook(hook, "codex", None, false);
}

pub fn process_vscode_json(input: &[u8]) {
    let Ok(payload) = serde_json::from_slice::<serde_json::Value>(input) else {
        return;
    };
    let Some(event_name) = payload.get("hook_event_name").and_then(|v| v.as_str()) else {
        return;
    };
    if event_name == "SessionStart" {
        return;
    }
    if !matches!(event_name, "PreToolUse" | "PostToolUse" | "Stop") {
        return;
    }
    let tool_name = payload.get("tool_name").and_then(|v| v.as_str());
    let mut target_paths = None;
    let require_window = match (event_name, tool_name) {
        ("Stop", _) => false,
        ("PreToolUse" | "PostToolUse", Some("create_file")) => {
            target_paths = Some(
                payload
                    .get("tool_input")
                    .and_then(|v| v.get("filePath"))
                    .and_then(|v| v.as_str())
                    .map(|path| vec![path.to_owned()])
                    .unwrap_or_default(),
            );
            true
        }
        ("PreToolUse" | "PostToolUse", Some("apply_patch")) => {
            target_paths = Some(
                payload
                    .get("tool_input")
                    .and_then(|v| v.get("input"))
                    .and_then(|v| v.as_str())
                    .map(parse_apply_patch_paths)
                    .unwrap_or_default(),
            );
            true
        }
        ("PreToolUse" | "PostToolUse", Some("run_in_terminal")) => false,
        ("PreToolUse" | "PostToolUse", Some(name)) if is_read_only_vscode_tool(name) => return,
        ("PreToolUse" | "PostToolUse", Some(name)) => {
            append_vscode_diagnostic(
                payload.get("cwd").and_then(|v| v.as_str()).map(Path::new),
                name,
                event_name == "PreToolUse",
            );
            return;
        }
        _ => return,
    };
    let hook = HookInput {
        hook_event_name: Some(event_name.to_owned()),
        session_id: payload
            .get("session_id")
            .and_then(|v| v.as_str())
            .map(str::to_owned),
        turn_id: None,
        cwd: payload
            .get("cwd")
            .and_then(|v| v.as_str())
            .map(str::to_owned),
        model: payload
            .get("model")
            .and_then(|v| v.as_str())
            .map(str::to_owned),
        tool_name: tool_name.map(str::to_owned),
        tool_use_id: payload
            .get("tool_use_id")
            .and_then(|v| v.as_str())
            .map(str::to_owned),
        tool_input: payload.get("tool_input").cloned(),
    };
    process_hook(hook, "vscode-copilot", target_paths, require_window);
}

fn is_read_only_vscode_tool(name: &str) -> bool {
    matches!(
        name,
        "file_search"
            | "get_errors"
            | "grep_search"
            | "list_dir"
            | "read_file"
            | "memory"
            | "vscode_askQuestions"
            | "semantic_search"
    )
}

fn parse_apply_patch_paths(patch: &str) -> Vec<String> {
    patch
        .lines()
        .filter_map(|line| {
            ["*** Update File: ", "*** Add File: ", "*** Delete File: "]
                .iter()
                .find_map(|prefix| line.strip_prefix(prefix))
        })
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .map(str::to_owned)
        .collect()
}

fn structured_edit_allowed(adapter: &str, operation: Option<&str>) -> bool {
    matches!(
        (adapter, operation),
        ("codex", Some("apply_patch"))
            | (
                "vscode-copilot",
                Some(
                    "create_file"
                        | "apply_patch"
                        | "insert_edit_into_file"
                        | "replace_string_in_file",
                ),
            )
    )
}

fn structured_added_lines(
    adapter: &str,
    operation: Option<&str>,
    input: &serde_json::Value,
) -> Option<Vec<String>> {
    let operation = operation?;
    let content = match (adapter, operation) {
        ("vscode-copilot", "create_file") => input.get("content")?.as_str()?,
        (_, "apply_patch") => input
            .get("input")
            .or_else(|| input.get("patch"))
            .and_then(serde_json::Value::as_str)?,
        ("vscode-copilot", "insert_edit_into_file") => input
            .get("code")
            .or_else(|| input.get("text"))
            .and_then(serde_json::Value::as_str)?,
        ("vscode-copilot", "replace_string_in_file") => input.get("newString")?.as_str()?,
        _ => return None,
    };
    let lines = if operation == "create_file" {
        Vec::new()
    } else if operation == "apply_patch" {
        content
            .lines()
            .filter_map(|line| line.strip_prefix('+'))
            .filter(|line| !line.starts_with("++"))
            .map(str::to_owned)
            .collect()
    } else {
        content.lines().map(str::to_owned).collect()
    };
    Some(lines)
}

fn structured_input(
    adapter: &str,
    operation: Option<&str>,
    input: &serde_json::Value,
) -> Option<String> {
    let operation = operation?;
    let field = match (adapter, operation) {
        ("vscode-copilot", "create_file") => "content",
        (_, "apply_patch") => {
            return input
                .get("input")
                .or_else(|| input.get("patch"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned);
        }
        ("vscode-copilot", "insert_edit_into_file") => "code",
        ("vscode-copilot", "replace_string_in_file") => "newString",
        _ => return None,
    };
    input.get(field)?.as_str().map(str::to_owned)
}

fn append_vscode_diagnostic(cwd: Option<&Path>, tool_name: &str, pre_event: bool) {
    if !pre_event {
        return;
    }
    let Some(home) = store_home() else { return };
    let path = cwd
        .filter(|path| path.is_dir())
        .and_then(|cwd| repo_info(cwd, &home))
        .and_then(|repo| {
            repo.repo_id
                .map(|id| home.join("repos").join(id).join("markers.jsonl"))
        })
        .unwrap_or_else(|| home.join("inbox.jsonl"));
    let Some(parent) = path.parent() else { return };
    if ensure_private_dir(parent).is_err() {
        return;
    }
    let mut bytes = serde_json::to_vec(&serde_json::json!({
        "event": "unsupported_tool",
        "tool_name": tool_name,
    }))
    .unwrap_or_default();
    bytes.push(b'\n');
    if let Ok(mut file) = OpenOptions::new()
        .create(true)
        .read(true)
        .append(true)
        .open(path)
    {
        let mut last = [0];
        let torn = file
            .seek(SeekFrom::End(-1))
            .ok()
            .and_then(|_| file.read_exact(&mut last).ok())
            .is_some()
            && last[0] != b'\n';
        if !torn || file.write_all(b"\n").is_ok() {
            let _ = file.write_all(&bytes);
        }
    }
}

fn process_hook(
    hook: HookInput,
    tool_label: &str,
    target_paths: Option<Vec<String>>,
    require_window: bool,
) {
    let Some(event_name) = hook.hook_event_name.as_deref() else {
        return;
    };
    let cwd = hook
        .cwd
        .as_deref()
        .map(Path::new)
        .filter(|cwd| cwd.is_dir());
    if cwd.is_none() && tool_label != "vscode-copilot" {
        return;
    }
    let Some(home) = store_home() else { return };
    let session = clean_id(hook.session_id.as_deref().unwrap_or("unknown"));
    let tool_id = hook.tool_use_id.as_deref().map(str::to_owned);
    let discovery_start = Instant::now();
    let repo = cwd.and_then(|cwd| repo_info(cwd, &home)).or_else(|| {
        target_paths.as_ref()?.iter().find_map(|path| {
            let path = PathBuf::from(path);
            let path = if path.is_absolute() {
                path
            } else {
                cwd?.join(path)
            };
            let start = if path.is_dir() {
                path
            } else {
                path.parent()?.to_path_buf()
            };
            repo_info(&start, &home)
        })
    });
    let Some(mut repo) = repo else {
        add_ns(&TIMINGS.repo_discovery_ns, discovery_start.elapsed());
        return;
    };
    add_ns(&TIMINGS.repo_discovery_ns, discovery_start.elapsed());
    let target_paths = target_paths.map(|paths| {
        paths
            .iter()
            .filter_map(|path| repo_relative_path(&repo.root, path))
            .collect::<Vec<_>>()
    });
    let marker_context = MarkerContext {
        home: home.clone(),
        repo_id: repo.repo_id.clone(),
        session_id: Some(session.clone()),
        tool_use_id: tool_id.clone(),
        hook_event_name: Some(event_name.to_owned()),
    };
    set_marker_context(&marker_context);
    if repo.shallow {
        let mut e = Event::new(
            "shallow_repo_skipped",
            Some(event_name),
            repo.repo_id.as_deref(),
            Some(&session),
            tool_id.as_deref(),
        );
        e.reason = Some("shallow_repository".into());
        append_marker(&marker_context, &e);
        return;
    }
    let Some(repo_id) = repo.repo_id.take() else {
        return;
    };
    let store = home.join("repos").join(&repo_id);
    if ensure_private_dir(&store).is_err() {
        return;
    }
    for dir in [
        store.join("blobs"),
        store.join("events"),
        store.join("shadow"),
        store.join("state"),
    ] {
        if ensure_private_dir(&dir).is_err() {
            return;
        }
    }
    let event_path = store.join("events").join(format!("{session}.jsonl"));
    let lock_path = store.join("repo.lock");
    let lock_start = Instant::now();
    let Ok(lock_file) = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(lock_path)
    else {
        return;
    };
    let mut lock = RwLock::new(lock_file);
    let started = Instant::now();
    let guard = loop {
        match lock.try_write() {
            Ok(g) => break Some(g),
            Err(_) if started.elapsed() < LOCK_BUDGET => {
                std::thread::sleep(Duration::from_millis(8))
            }
            Err(_) => break None,
        }
    };
    let Some(_guard) = guard else {
        add_ns(&TIMINGS.lock_ns, lock_start.elapsed());
        let mut e = Event::new(
            "lock_timeout",
            Some(event_name),
            Some(&repo_id),
            Some(&session),
            tool_id.as_deref(),
        );
        e.degraded = Some(true);
        e.reason = Some("lock_timeout".into());
        append_marker(&marker_context, &e);
        return;
    };
    add_ns(&TIMINGS.lock_ns, lock_start.elapsed());
    let state_path = store.join("state/repo.json");
    let mut state: RepoState = read_json(&state_path).unwrap_or_default();
    expire_state(&mut state);
    let shadow_path = store.join("shadow").join(format!(
        "{}.json",
        hash(repo.git_dir.to_string_lossy().as_bytes())
    ));
    let mut shadow: Shadow = read_json(&shadow_path).unwrap_or_default();
    let head = repo.head.clone();
    let branch = repo.branch.clone();
    let mut derive_after_hook = false;
    match event_name {
        "PreToolUse" => {
            let scan = timed_scan(&repo.root, &store.join("blobs"), &mut shadow, SCAN_BUDGET);
            let (changes, incomplete, bulk_count, bulk_remaining) = match scan {
                Ok(v) => (v.changes, v.incomplete, v.bulk_count, v.bulk_remaining),
                Err(_) => (Vec::new(), true, None, None),
            };
            let after_incomplete = pending_external_flag(&mut state, incomplete);
            if incomplete {
                append_incomplete(
                    &event_path,
                    &repo_id,
                    event_name,
                    &session,
                    tool_id.as_deref(),
                );
            }
            if let Some(count) = bulk_count {
                let mut e = Event::new(
                    "bulk_change",
                    Some(event_name),
                    Some(&repo_id),
                    Some(&session),
                    tool_id.as_deref(),
                );
                e.dirty_count = Some(count);
                e.remaining = bulk_remaining;
                e.degraded = Some(true);
                e.reason = Some("bulk_change".into());
                e.bulk_partial = Some(true);
                append_line(&event_path, &e);
            } else {
                let mut event_batch = Vec::with_capacity(changes.len());
                for change in changes {
                    let mut e = change_event(
                        "external_change",
                        change,
                        event_name,
                        &repo_id,
                        &session,
                        tool_id.as_deref(),
                        after_incomplete,
                    );
                    e.degraded = e.reason.is_some().then_some(true);
                    event_batch.push(e);
                }
                append_events(&event_path, &event_batch);
            }
            let now = now_ns();
            let overlap = state
                .windows
                .iter()
                .any(|w| w.session_id != session && w.opened_at <= now);
            if overlap {
                for w in &mut state.windows {
                    if w.session_id != session {
                        w.overlap = true;
                    }
                }
            }
            let window = WindowState {
                session_id: session.clone(),
                tool_use_id: tool_id.clone(),
                target_paths: target_paths.clone(),
                opened_at: now,
                head: head.clone(),
                branch: branch.clone(),
                overlap,
                bulk_count,
                bulk_remaining,
            };
            let mut e = Event::new(
                "window_open",
                Some(event_name),
                Some(&repo_id),
                Some(&session),
                tool_id.as_deref(),
            );
            e.window_open = Some(now);
            e.overlap = Some(overlap);
            e.head = Some(head);
            e.branch = Some(branch);
            e.degraded = Some(incomplete || bulk_count.is_some());
            if bulk_count.is_some() {
                e.degraded = Some(true);
                e.reason = Some("bulk_change".into());
                e.bulk_partial = Some(true);
            }
            append_line(&event_path, &e);
            state.windows.push(window);
        }
        "PostToolUse" => {
            let scan = timed_scan(&repo.root, &store.join("blobs"), &mut shadow, SCAN_BUDGET);
            let (changes, incomplete, bulk_count, bulk_remaining) = match scan {
                Ok(v) => (v.changes, v.incomplete, v.bulk_count, v.bulk_remaining),
                Err(_) => (Vec::new(), true, None, None),
            };
            if incomplete {
                state.scan_incomplete_pending = true;
                append_incomplete(
                    &event_path,
                    &repo_id,
                    event_name,
                    &session,
                    tool_id.as_deref(),
                );
            }
            let (close_head, close_branch) = head_state(&repo);
            let ix = state
                .windows
                .iter()
                .rposition(|w| w.session_id == session && w.tool_use_id == tool_id);
            let window = ix.map(|i| state.windows.remove(i));
            let missing_window = require_window && window.is_none();
            let scoped_paths = window
                .as_ref()
                .and_then(|window| window.target_paths.as_ref())
                .or(target_paths.as_ref());
            let opened = window.as_ref().map(|w| w.opened_at);
            let prior_bulk = window.as_ref().and_then(|w| w.bulk_count);
            let bulk_window = prior_bulk.is_some() || bulk_count.is_some();
            let overlap = window.as_ref().is_some_and(|w| w.overlap)
                || state
                    .windows
                    .iter()
                    .any(|o| o.session_id != session && o.opened_at <= now_ns());
            let head_moved = window
                .as_ref()
                .is_some_and(|w| w.head != close_head || w.branch != close_branch);
            let head_move = head_moved
                .then(|| {
                    let (old_head, old_branch) = window
                        .as_ref()
                        .map(|w| (w.head.as_str(), w.branch.as_str()))
                        .unwrap_or(("unknown", ""));
                    head_move_summary(&repo.root, old_head, &close_head)
                        .map(|summary| (old_head.to_owned(), old_branch.to_owned(), summary))
                })
                .flatten();
            if bulk_count.is_some() && prior_bulk.is_none() {
                let mut e = Event::new(
                    "bulk_change",
                    Some(event_name),
                    Some(&repo_id),
                    Some(&session),
                    tool_id.as_deref(),
                );
                e.dirty_count = bulk_count;
                e.remaining = bulk_remaining;
                e.degraded = Some(true);
                e.reason = Some("bulk_change".into());
                e.bulk_partial = Some(true);
                append_line(&event_path, &e);
            }
            let command_sha = hook
                .tool_input
                .as_ref()
                .and_then(|v| v.get("command"))
                .and_then(serde_json::Value::as_str)
                .map(|v| hash(v.as_bytes()));
            let command_length = hook
                .tool_input
                .as_ref()
                .and_then(|v| v.get("command"))
                .and_then(serde_json::Value::as_str)
                .map(str::len);
            let now = now_ns();
            let mut event_batch = Vec::with_capacity(changes.len());
            for change in changes
                .into_iter()
                .filter(|change| scoped_paths.is_none_or(|paths| paths.contains(&change.path)))
            {
                let duplicate = tool_id.as_ref().is_some_and(|id| {
                    state.recent_keys.iter().any(|k| {
                        k.tool_use_id == *id && k.path == change.path && k.after_sha == change.after
                    })
                });
                if duplicate {
                    continue;
                }
                let kind =
                    if head_moved || bulk_window || change.degraded.is_some() || missing_window {
                        "external_change"
                    } else {
                        "ai_change"
                    };
                let mut e = change_event(
                    kind,
                    change,
                    event_name,
                    &repo_id,
                    &session,
                    tool_id.as_deref(),
                    false,
                );
                e.turn_id = hook.turn_id.clone();
                e.model = hook.model.clone();
                e.tool = Some(tool_label.into());
                e.operation = hook.tool_name.clone();
                e.structured_edit = Some(structured_edit_allowed(
                    tool_label,
                    hook.tool_name.as_deref(),
                ));
                e.structured_added_lines = hook
                    .tool_input
                    .as_ref()
                    .and_then(|input| {
                        structured_added_lines(tool_label, hook.tool_name.as_deref(), input)
                    })
                    .unwrap_or_default();
                e.structured_input_sha256 = hook
                    .tool_input
                    .as_ref()
                    .and_then(|input| {
                        structured_input(tool_label, hook.tool_name.as_deref(), input)
                    })
                    .map(|payload| hash(payload.replace("\r\n", "\n").as_bytes()));
                e.command_sha256 = command_sha.clone();
                e.command_length = command_length;
                e.window_open = opened;
                e.window_close = Some(now);
                e.overlap = Some(overlap);
                e.head = Some(close_head.clone());
                e.branch = Some(close_branch.clone());
                if missing_window {
                    e.reason = Some("before_unavailable".into());
                    e.degraded = Some(true);
                    e.line_ranges = None;
                } else if head_moved {
                    e.reason = Some("head_moved".into());
                    e.head_moved = Some(true);
                } else if bulk_window {
                    e.reason = Some("bulk_change".into());
                    e.degraded = Some(true);
                    e.bulk_partial = Some(true);
                }
                e.degraded = e.reason.is_some().then_some(true);
                if let Some(id) = &tool_id {
                    state.recent_keys.push(RecentKey {
                        tool_use_id: id.clone(),
                        path: e.path.clone().unwrap_or_default(),
                        after_sha: e.after_sha.clone(),
                        timestamp: now,
                    });
                }
                event_batch.push(e);
            }
            append_events(&event_path, &event_batch);
            if let Some((old_head, old_branch, summary)) = head_move {
                let mut e = Event::new(
                    "head_move",
                    Some(event_name),
                    Some(&repo_id),
                    Some(&session),
                    tool_id.as_deref(),
                );
                e.old_head = Some(old_head);
                e.new_head = Some(close_head.clone());
                e.branch_before = Some(old_branch);
                e.branch_after = Some(close_branch.clone());
                e.file_count = Some(summary.file_count);
                e.paths = Some(summary.paths);
                e.truncated = Some(summary.truncated);
                e.commit_in_window = Some(summary.commit_in_window);
                e.head_moved = Some(true);
                e.degraded = Some(true);
                derive_after_hook = append_line(&event_path, &e);
            }
            let mut e = Event::new(
                "window_close",
                Some(event_name),
                Some(&repo_id),
                Some(&session),
                tool_id.as_deref(),
            );
            e.window_open = opened;
            e.window_close = Some(now);
            e.head = Some(close_head);
            e.branch = Some(close_branch);
            e.head_moved = Some(head_moved);
            e.degraded = Some(incomplete || bulk_window);
            if bulk_window {
                e.degraded = Some(true);
                e.reason = Some("bulk_change".into());
                e.bulk_partial = Some(true);
            }
            append_line(&event_path, &e);
        }
        "Stop" => {
            let mut e = Event::new(
                "turn_end",
                Some(event_name),
                Some(&repo_id),
                Some(&session),
                tool_id.as_deref(),
            );
            e.turn_id = hook.turn_id;
            append_line(&event_path, &e);
        }
        _ => {}
    }
    cap_state(&mut state);
    let _ = atomic_json(&state_path, &state, true);
    let _ = atomic_json(&shadow_path, &shadow, true);
    if derive_after_hook {
        trigger_auto_derive(&repo.root);
    }
}

fn repo_info(cwd: &Path, home: &Path) -> Option<RepoInfo> {
    let (root_path, git_dir, common_dir) = find_git_dirs(cwd)?;
    if normalized_path(home).starts_with(normalized_path(&root_path)) {
        return None;
    }
    let shallow = [git_dir.join("shallow"), common_dir.join("shallow")]
        .iter()
        .any(|p| fs::metadata(p).is_ok_and(|m| m.len() > 0));
    let (head, branch) =
        read_head(&git_dir, &common_dir).unwrap_or_else(|| fallback_head(&root_path));
    let cache_path = home.join("repo-cache").join(format!(
        "{}.json",
        hash(git_dir.to_string_lossy().as_bytes())
    ));
    let git_dir_created_ns = fs::metadata(&git_dir)
        .ok()
        .and_then(|m| m.created().ok())
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos());
    let cached: Option<RepoCache> = read_json(&cache_path);
    if shallow {
        return Some(RepoInfo {
            root: root_path,
            git_dir,
            common_dir,
            repo_id: cached.map(|c| c.repo_id),
            shallow,
            head,
            branch,
        });
    }
    let needs_root_check = cached.as_ref().is_some_and(|cache| {
        git_dir_created_ns.is_none() || cache.git_dir_created_ns != git_dir_created_ns
    });
    let should_compute = cached.is_none() || needs_root_check;
    let roots = if should_compute {
        let mut roots: Vec<String> = git_text(
            &root_path,
            &["rev-list", "--first-parent", "--max-parents=0", "HEAD"],
        )?
        .lines()
        .map(str::to_owned)
        .collect();
        roots.sort();
        Some(roots)
    } else {
        None
    };
    let cached_still_matches = cached.as_ref().is_some_and(|cache| {
        if let Some(created) = git_dir_created_ns {
            cache.git_dir_created_ns == Some(created)
        } else {
            roots
                .as_ref()
                .is_some_and(|current| cache.root_check == current.join("\n"))
        }
    });
    let repo_id = if cached_still_matches {
        cached.as_ref().map(|c| c.repo_id.clone())
    } else {
        let roots = roots?;
        if roots.is_empty() {
            return None;
        }
        let root_check = roots.join("\n");
        let id = hash(root_check.as_bytes());
        let _ = atomic_json(
            &cache_path,
            &RepoCache {
                repo_id: id.clone(),
                root_check,
                git_dir_created_ns,
            },
            false,
        );
        Some(id)
    };
    Some(RepoInfo {
        root: root_path,
        git_dir,
        common_dir,
        repo_id,
        shallow,
        head,
        branch,
    })
}

fn normalized_path(path: &Path) -> PathBuf {
    if let Ok(value) = path.canonicalize() {
        return value;
    }
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    };
    let mut parts = Vec::new();
    for component in absolute.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                parts.pop();
            }
            other => parts.push(other.as_os_str().to_owned()),
        }
    }
    parts.into_iter().collect()
}

fn repo_relative_path(root: &Path, value: &str) -> Option<String> {
    let value = value.replace('\\', "/");
    let path = PathBuf::from(&value);
    let candidate = if path.is_absolute() {
        path
    } else {
        root.join(path)
    };
    let candidate = normalize_drive_letter(canonicalized_normalized_path(&candidate));
    let root = normalize_drive_letter(normalized_path(root));
    let relative = candidate.strip_prefix(root).ok()?;
    let value = relative.to_string_lossy().replace('\\', "/");
    (!value.is_empty() && value != "." && !value.starts_with("../")).then_some(value)
}

fn canonicalized_normalized_path(path: &Path) -> PathBuf {
    let normalized = lexical_normalized_path(path);
    if let Ok(canonical) = normalized.canonicalize() {
        return canonical;
    }
    let mut existing = normalized.as_path();
    let mut missing = Vec::new();
    while !existing.exists() {
        let Some(name) = existing.file_name() else {
            return normalized;
        };
        missing.push(name.to_os_string());
        let Some(parent) = existing.parent() else {
            return normalized;
        };
        existing = parent;
    }
    let mut canonical = existing
        .canonicalize()
        .unwrap_or_else(|_| existing.to_path_buf());
    for component in missing.iter().rev() {
        canonical.push(component);
    }
    canonical
}

fn lexical_normalized_path(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    };
    let mut parts = Vec::new();
    for component in absolute.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                parts.pop();
            }
            other => parts.push(other.as_os_str().to_owned()),
        }
    }
    parts.into_iter().collect()
}

fn normalize_drive_letter(path: PathBuf) -> PathBuf {
    let value = path.to_string_lossy();
    let bytes = value.as_bytes();
    if bytes.len() > 1 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic() {
        let mut normalized = value.into_owned();
        let drive = normalized[0..1].to_ascii_lowercase();
        normalized.replace_range(0..1, &drive);
        PathBuf::from(normalized)
    } else {
        path
    }
}

fn find_git_dirs(cwd: &Path) -> Option<(PathBuf, PathBuf, PathBuf)> {
    let start = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    for root in start.ancestors() {
        let marker = root.join(".git");
        if marker.is_dir() {
            let git_dir = marker.canonicalize().ok()?;
            let common_dir = read_common_dir(&git_dir);
            return Some((root.to_path_buf(), git_dir, common_dir));
        }
        if marker.is_file() {
            let content = fs::read_to_string(&marker).ok()?;
            let value = content.trim().strip_prefix("gitdir:")?.trim();
            let path = PathBuf::from(value);
            let git_dir = if path.is_absolute() {
                path
            } else {
                root.join(path)
            }
            .canonicalize()
            .ok()?;
            let common_dir = read_common_dir(&git_dir);
            return Some((root.to_path_buf(), git_dir, common_dir));
        }
    }
    None
}

fn read_common_dir(git_dir: &Path) -> PathBuf {
    let path = git_dir.join("commondir");
    let Ok(text) = fs::read_to_string(path) else {
        return git_dir.to_path_buf();
    };
    let value = PathBuf::from(text.trim());
    let value = if value.is_absolute() {
        value
    } else {
        git_dir.join(value)
    };
    value.canonicalize().unwrap_or(value)
}

fn read_head(git_dir: &Path, common_dir: &Path) -> Option<(String, String)> {
    let head = fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let value = head.trim();
    if let Some(reference) = value.strip_prefix("ref:") {
        let reference = reference.trim();
        let branch = reference
            .strip_prefix("refs/heads/")
            .unwrap_or("")
            .to_owned();
        let sha = read_ref(git_dir, common_dir, reference)?;
        return Some((sha, branch));
    }
    is_git_hash(value).then(|| (format!("detached:{value}"), String::new()))
}

fn read_ref(git_dir: &Path, common_dir: &Path, reference: &str) -> Option<String> {
    for dir in [git_dir, common_dir] {
        if let Ok(value) = fs::read_to_string(dir.join(reference)) {
            let value = value.trim();
            if is_git_hash(value) {
                return Some(value.to_owned());
            }
        }
    }
    for packed in [git_dir.join("packed-refs"), common_dir.join("packed-refs")] {
        let Ok(contents) = fs::read_to_string(packed) else {
            continue;
        };
        for line in contents.lines() {
            if line.starts_with('#') || line.starts_with('^') {
                continue;
            }
            let mut parts = line.split_whitespace();
            let (Some(sha), Some(name)) = (parts.next(), parts.next()) else {
                continue;
            };
            if name == reference && is_git_hash(sha) {
                return Some(sha.to_owned());
            }
        }
    }
    None
}

fn is_git_hash(value: &str) -> bool {
    (value.len() == 40 || value.len() == 64) && value.bytes().all(|b| b.is_ascii_hexdigit())
}

fn fallback_head(root: &Path) -> (String, String) {
    let head = git_text(root, &["rev-parse", "HEAD"]).unwrap_or_else(|| "unknown".into());
    let branch = git_text(root, &["symbolic-ref", "--quiet", "--short", "HEAD"])
        .unwrap_or_default()
        .trim()
        .to_owned();
    let head = head.trim();
    let value = if branch.is_empty() {
        format!("detached:{head}")
    } else {
        head.to_owned()
    };
    (value, branch)
}

fn head_state(repo: &RepoInfo) -> (String, String) {
    read_head(&repo.git_dir, &repo.common_dir).unwrap_or_else(|| fallback_head(&repo.root))
}
fn git_text(cwd: &Path, args: &[&str]) -> Option<String> {
    let out = git_output(cwd, args, None).ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}
fn git_output(
    cwd: &Path,
    args: &[&str],
    input: Option<&[u8]>,
) -> std::io::Result<std::process::Output> {
    TIMINGS.git_processes.fetch_add(1, Ordering::Relaxed);
    let mut command = Command::new("git");
    command
        .arg("-c")
        .arg("core.quotepath=false")
        .args(args)
        .current_dir(cwd)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stderr(Stdio::null());
    configure_process_command(&mut command, false);
    if let Some(input) = input {
        command.stdin(Stdio::piped()).stdout(Stdio::piped());
        let mut child = command.spawn()?;
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(input);
        }
        child.wait_with_output()
    } else {
        command.stdin(Stdio::null()).output()
    }
}

struct ScanResult {
    changes: Vec<FileChange>,
    incomplete: bool,
    bulk_count: Option<usize>,
    bulk_remaining: Option<usize>,
}

fn scan(
    root: &Path,
    blobs: &Path,
    shadow: &mut Shadow,
    budget: Duration,
) -> std::io::Result<ScanResult> {
    let start = Instant::now();
    let status_start = Instant::now();
    let status = git_output(
        root,
        &["status", "--porcelain=v2", "-z", "--untracked-files=all"],
        None,
    )?;
    add_ns(&TIMINGS.git_status_ns, status_start.elapsed());
    if !status.status.success() {
        return Err(std::io::Error::other("git status failed"));
    }
    let dirty = parse_status(&status.stdout);
    let dirty_set: HashSet<String> = dirty.iter().map(|d| d.path.clone()).collect();
    shadow.files.retain(|path, _| dirty_set.contains(path));

    let mut candidates = Vec::new();
    for d in &dirty {
        let path = d.path.replace('\\', "/");
        if let Some(s) = shadow.files.get(&path) {
            let full = root.join(raw_path(d.path.as_bytes()));
            match fs::symlink_metadata(&full) {
                Ok(m) => {
                    if m.file_type().is_symlink() {
                        candidates.push(d.clone());
                    } else {
                        let mtime_ns = m
                            .modified()
                            .ok()
                            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                            .map_or(0, |d| d.as_nanos());
                        if s.deleted || s.size != m.len() || s.mtime_ns != mtime_ns {
                            candidates.push(d.clone());
                        }
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    if !s.deleted {
                        candidates.push(d.clone());
                    }
                }
                Err(_) => {
                    candidates.push(d.clone());
                }
            }
        } else {
            candidates.push(d.clone());
        }
    }

    let total_candidates = candidates.len();
    let is_bulk = total_candidates > MAX_DIRTY_PATHS;
    let to_process = if is_bulk {
        &candidates[..MAX_DIRTY_PATHS]
    } else {
        &candidates[..]
    };
    let remaining = total_candidates.saturating_sub(MAX_DIRTY_PATHS);

    if is_bulk {
        for d in to_process {
            if start.elapsed() >= budget {
                break;
            }
            let path = d.path.replace('\\', "/");
            let excluded = is_excluded(&path);
            let full = root.join(raw_path(d.path.as_bytes()));
            let meta = match fs::symlink_metadata(&full) {
                Ok(m) if m.file_type().is_symlink() => {
                    shadow.files.remove(&path);
                    continue;
                }
                Ok(m) => Some(m),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(_) => continue,
            };
            let Some(meta) = meta else {
                shadow.files.insert(
                    path,
                    FileState {
                        size: 0,
                        mtime_ns: 0,
                        sha256: String::new(),
                        excluded,
                        deleted: true,
                    },
                );
                continue;
            };
            if !meta.file_type().is_file() || meta.len() > MAX_FILE {
                shadow.files.remove(&path);
                continue;
            }
            let mtime_ns = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_nanos());
            let f = match File::open(&full) {
                Ok(f) => f,
                Err(_) => continue,
            };
            let mut after_bytes = Vec::with_capacity(meta.len() as usize);
            if f.take(MAX_FILE + 1).read_to_end(&mut after_bytes).is_err() {
                continue;
            }
            if after_bytes.len() as u64 > MAX_FILE
                || after_bytes[..after_bytes.len().min(8192)].contains(&0)
            {
                shadow.files.remove(&path);
                continue;
            }
            let after_sha = hash(&after_bytes);
            // Bulk mode writes hash-only shadow entries for dirty paths without storing the blob,
            // so later derivation must treat the prior content as unknown rather than empty.
            shadow.files.insert(
                path,
                FileState {
                    size: after_bytes.len() as u64,
                    mtime_ns,
                    sha256: after_sha,
                    excluded,
                    deleted: false,
                },
            );
        }
        return Ok(ScanResult {
            changes: Vec::new(),
            incomplete: start.elapsed() >= budget,
            bulk_count: Some(dirty.len()),
            bulk_remaining: Some(remaining),
        });
    }

    let mut head_ids = Vec::new();
    for d in to_process {
        if !d.untracked
            && !d.unmerged
            && !is_excluded(&d.path)
            && !shadow.files.contains_key(&d.path)
        {
            if let Some(id) = &d.head_blob {
                head_ids.push(id.clone());
            }
        }
    }
    if start.elapsed() >= budget {
        return Ok(ScanResult {
            changes: Vec::new(),
            incomplete: true,
            bulk_count: None,
            bulk_remaining: None,
        });
    }
    let sizes = check_blob_sizes(root, &head_ids).unwrap_or_default();
    let ids_to_fetch: Vec<String> = head_ids
        .into_iter()
        .filter(|id| sizes.get(id).is_some_and(|&s| s <= MAX_FILE as usize))
        .collect();
    let head_contents = cat_blobs(root, &ids_to_fetch).unwrap_or_default();

    let mut changes = Vec::new();
    let mut incomplete = start.elapsed() > budget;
    for d in to_process {
        if start.elapsed() >= budget {
            incomplete = true;
            break;
        }
        let path = d.path.replace('\\', "/");
        let excluded = is_excluded(&path);
        if d.unmerged {
            changes.push(FileChange {
                path,
                before: None,
                after: None,
                excluded,
                degraded: Some("unmerged".into()),
                line_ranges: None,
                deleted_at: Vec::new(),
            });
            continue;
        }
        let previous = shadow.files.get(&path).cloned();
        let full = root.join(raw_path(d.path.as_bytes()));
        let meta = match fs::symlink_metadata(&full) {
            Ok(m) if m.file_type().is_symlink() => {
                shadow.files.remove(&path);
                continue;
            }
            Ok(m) => Some(m),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => {
                let before_sha_hint = previous.as_ref().map(|s| s.sha256.clone()).or_else(|| {
                    d.head_blob
                        .as_ref()
                        .and_then(|id| head_contents.get(id))
                        .map(|bytes| hash(bytes))
                });
                changes.push(file_error(path, before_sha_hint, "metadata", &e));
                continue;
            }
        };

        let before_content = if excluded {
            BeforeContent::Unavailable
        } else if let Some(prev) = previous.as_ref() {
            if prev.deleted {
                BeforeContent::KnownEmpty
            } else {
                match read_blob(&blobs.join(&prev.sha256), &prev.sha256) {
                    Some(bytes) => BeforeContent::Known(bytes),
                    None => BeforeContent::Unavailable,
                }
            }
        } else if d.unmerged || d.renamed {
            BeforeContent::Unavailable
        } else if d.untracked || d.head_blob.is_none() {
            BeforeContent::KnownEmpty
        } else if let Some(ref blob_id) = d.head_blob {
            match head_contents.get(blob_id) {
                Some(bytes) => BeforeContent::Known(bytes.clone()),
                None => BeforeContent::Unavailable,
            }
        } else {
            BeforeContent::Unavailable
        };

        let before_sha = match &before_content {
            BeforeContent::Known(bytes) => Some(hash(bytes)),
            BeforeContent::KnownEmpty => None,
            BeforeContent::Unavailable => previous.as_ref().map(|s| s.sha256.clone()),
        };

        let Some(meta) = meta else {
            if previous.as_ref().is_some_and(|s| s.deleted) {
                continue;
            }
            match before_content {
                BeforeContent::Unavailable if !excluded => {
                    changes.push(FileChange {
                        path: path.clone(),
                        before: before_sha.clone(),
                        after: None,
                        excluded,
                        degraded: Some("before_unavailable".into()),
                        line_ranges: None,
                        deleted_at: Vec::new(),
                    });
                }
                BeforeContent::Known(ref old_bytes) => {
                    let (_, deleted_at) = diff_changes_and_deletions(old_bytes, b"");
                    changes.push(FileChange {
                        path: path.clone(),
                        before: before_sha.clone(),
                        after: None,
                        excluded,
                        degraded: None,
                        line_ranges: Some(Vec::new()),
                        deleted_at,
                    });
                }
                _ => {
                    changes.push(FileChange {
                        path: path.clone(),
                        before: before_sha.clone(),
                        after: None,
                        excluded,
                        degraded: None,
                        line_ranges: if excluded { None } else { Some(Vec::new()) },
                        deleted_at: Vec::new(),
                    });
                }
            }
            let old = previous.unwrap_or(FileState {
                size: 0,
                mtime_ns: 0,
                sha256: before_sha.unwrap_or_default(),
                excluded,
                deleted: false,
            });
            shadow.files.insert(
                path,
                FileState {
                    deleted: true,
                    ..old
                },
            );
            continue;
        };

        if !meta.file_type().is_file() || meta.len() > MAX_FILE {
            shadow.files.remove(&path);
            continue;
        }
        let mtime_ns = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_nanos());
        let f = match File::open(&full) {
            Ok(f) => f,
            Err(e) => {
                changes.push(file_error(path, before_sha, "open", &e));
                continue;
            }
        };
        let mut after_bytes = Vec::with_capacity(meta.len() as usize);
        if let Err(e) = f.take(MAX_FILE + 1).read_to_end(&mut after_bytes) {
            changes.push(file_error(path, before_sha, "read", &e));
            continue;
        }
        if after_bytes.len() as u64 > MAX_FILE
            || after_bytes[..after_bytes.len().min(8192)].contains(&0)
        {
            shadow.files.remove(&path);
            continue;
        }
        let after_sha = hash(&after_bytes);
        if before_sha.as_deref() != Some(&after_sha) {
            if !excluded {
                if let Err(e) = write_once(&blobs.join(&after_sha), &after_bytes) {
                    changes.push(file_error(path, before_sha, "snapshot", &e));
                    continue;
                }
            }
            match before_content {
                BeforeContent::Unavailable if !excluded => {
                    changes.push(FileChange {
                        path: path.clone(),
                        before: before_sha,
                        after: Some(after_sha.clone()),
                        excluded,
                        degraded: Some("before_unavailable".into()),
                        line_ranges: None,
                        deleted_at: Vec::new(),
                    });
                }
                BeforeContent::Known(ref old_bytes) => {
                    let (ranges, deleted_at) = diff_changes_and_deletions(old_bytes, &after_bytes);
                    changes.push(FileChange {
                        path: path.clone(),
                        before: before_sha,
                        after: Some(after_sha.clone()),
                        excluded,
                        degraded: None,
                        line_ranges: Some(ranges),
                        deleted_at,
                    });
                }
                BeforeContent::KnownEmpty => {
                    let (ranges, deleted_at) = diff_changes_and_deletions(b"", &after_bytes);
                    changes.push(FileChange {
                        path: path.clone(),
                        before: None,
                        after: Some(after_sha.clone()),
                        excluded,
                        degraded: None,
                        line_ranges: Some(ranges),
                        deleted_at,
                    });
                }
                _ => {
                    changes.push(FileChange {
                        path: path.clone(),
                        before: before_sha,
                        after: Some(after_sha.clone()),
                        excluded,
                        degraded: None,
                        line_ranges: None,
                        deleted_at: Vec::new(),
                    });
                }
            }
        }
        shadow.files.insert(
            path,
            FileState {
                size: after_bytes.len() as u64,
                mtime_ns,
                sha256: after_sha,
                excluded,
                deleted: false,
            },
        );
    }
    Ok(ScanResult {
        changes,
        incomplete,
        bulk_count: None,
        bulk_remaining: None,
    })
}

fn timed_scan(
    root: &Path,
    blobs: &Path,
    shadow: &mut Shadow,
    budget: Duration,
) -> std::io::Result<ScanResult> {
    let start = Instant::now();
    let before_status = TIMINGS.git_status_ns.load(Ordering::Relaxed);
    let before_writes = TIMINGS.writes_ns.load(Ordering::Relaxed);
    let result = scan(root, blobs, shadow, budget);
    let status = TIMINGS
        .git_status_ns
        .load(Ordering::Relaxed)
        .saturating_sub(before_status);
    let writes = TIMINGS
        .writes_ns
        .load(Ordering::Relaxed)
        .saturating_sub(before_writes);
    add_ns(
        &TIMINGS.hashing_snapshots_ns,
        start
            .elapsed()
            .saturating_sub(Duration::from_nanos(status.saturating_add(writes))),
    );
    result
}

fn parse_status(data: &[u8]) -> Vec<DirtyPath> {
    let mut out = Vec::new();
    let mut records = data.split(|b| *b == 0).filter(|x| !x.is_empty());
    while let Some(row) = records.next() {
        if row.starts_with(b"? ") {
            out.push(DirtyPath {
                path: String::from_utf8_lossy(&row[2..]).into_owned(),
                head_blob: None,
                untracked: true,
                unmerged: false,
                renamed: false,
            });
            continue;
        }
        let typ = row.first().copied().unwrap_or_default();
        let fields = match typ {
            b'1' => 8,
            b'2' => 9,
            b'u' => 10,
            _ => continue,
        };
        let parts = split_prefix(row, fields);
        if parts.len() <= fields {
            continue;
        }
        let head = String::from_utf8_lossy(parts[6]);
        let head_blob = (!head.bytes().all(|b| b == b'0')).then(|| head.into_owned());
        let path = String::from_utf8_lossy(parts[fields]).into_owned();
        out.push(DirtyPath {
            path,
            head_blob: if typ == b'2' || typ == b'u' {
                None
            } else {
                head_blob.clone()
            },
            untracked: false,
            unmerged: typ == b'u',
            renamed: typ == b'2',
        });
        if typ == b'2' {
            if let Some(old) = records.next() {
                out.push(DirtyPath {
                    path: String::from_utf8_lossy(old).into_owned(),
                    head_blob,
                    untracked: false,
                    unmerged: false,
                    renamed: true,
                });
            }
        }
    }
    out
}
fn split_prefix(row: &[u8], fields: usize) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut spaces = 0;
    for (i, b) in row.iter().enumerate() {
        if *b == b' ' {
            out.push(&row[start..i]);
            start = i + 1;
            spaces += 1;
            if spaces == fields {
                out.push(&row[start..]);
                return out;
            }
        }
    }
    out.push(&row[start..]);
    out
}
fn check_blob_sizes(root: &Path, ids: &[String]) -> Option<BTreeMap<String, usize>> {
    if ids.is_empty() {
        return Some(BTreeMap::new());
    }
    let mut input = Vec::new();
    for id in ids {
        input.extend_from_slice(id.as_bytes());
        input.push(b'\n');
    }
    let out = git_output(root, &["cat-file", "--batch-check"], Some(&input)).ok()?;
    if !out.status.success() {
        return None;
    }
    let mut data = out.stdout.as_slice();
    let mut result = BTreeMap::new();
    while !data.is_empty() {
        let nl = data.iter().position(|b| *b == b'\n')?;
        let header = &data[..nl];
        data = &data[nl + 1..];
        let fields: Vec<_> = header.split(|b| *b == b' ').collect();
        if fields.len() < 3 {
            continue;
        }
        let id = String::from_utf8_lossy(fields[0]).into_owned();
        if let Ok(len) = String::from_utf8_lossy(fields[2]).parse::<usize>() {
            result.insert(id, len);
        }
    }
    Some(result)
}

fn cat_blobs(root: &Path, ids: &[String]) -> Option<BTreeMap<String, Vec<u8>>> {
    if ids.is_empty() {
        return Some(BTreeMap::new());
    }
    let mut input = Vec::new();
    for id in ids {
        input.extend_from_slice(id.as_bytes());
        input.push(b'\n');
    }
    let out = git_output(root, &["cat-file", "--batch"], Some(&input)).ok()?;
    if !out.status.success() {
        return None;
    }
    let mut data = out.stdout.as_slice();
    let mut result = BTreeMap::new();
    while !data.is_empty() {
        let nl = data.iter().position(|b| *b == b'\n')?;
        let header = &data[..nl];
        data = &data[nl + 1..];
        let fields: Vec<_> = header.split(|b| *b == b' ').collect();
        if fields.len() < 3 {
            if fields.len() == 2 && fields[1] == b"missing" {
                continue;
            }
            return None;
        }
        let id = String::from_utf8_lossy(fields[0]).into_owned();
        let len = String::from_utf8_lossy(fields[2]).parse::<usize>().ok()?;
        if data.len() < len + 1 {
            return None;
        }
        result.insert(id, data[..len].to_vec());
        data = &data[len + 1..];
    }
    Some(result)
}

struct HeadMoveSummary {
    file_count: usize,
    paths: Vec<String>,
    truncated: bool,
    commit_in_window: bool,
}

fn head_move_summary(root: &Path, old_head: &str, new_head: &str) -> Option<HeadMoveSummary> {
    let old = old_head.strip_prefix("detached:").unwrap_or(old_head);
    let new = new_head.strip_prefix("detached:").unwrap_or(new_head);
    if old == "unknown" || new == "unknown" {
        return None;
    }
    let out = git_output(
        root,
        &["diff", "--no-renames", "--name-only", "-z", old, new],
        None,
    )
    .ok()?;
    if !out.status.success() {
        return None;
    }
    let all_paths: Vec<&[u8]> = out
        .stdout
        .split(|b| *b == 0)
        .filter(|p| !p.is_empty())
        .collect();
    let file_count = all_paths.len();
    let paths = all_paths
        .iter()
        .take(200)
        .map(|p| String::from_utf8_lossy(p).into_owned())
        .collect();
    let commit_in_window = git_output(root, &["cat-file", "-p", new], None)
        .ok()
        .filter(|o| o.status.success())
        .is_some_and(|o| {
            o.stdout
                .split(|b| *b == b'\n')
                .any(|line| line.strip_prefix(b"parent ") == Some(old.as_bytes()))
        });
    Some(HeadMoveSummary {
        file_count,
        paths,
        truncated: file_count > 200,
        commit_in_window,
    })
}

fn file_error(
    path: String,
    before: Option<String>,
    reason: &str,
    e: &std::io::Error,
) -> FileChange {
    let excluded = is_excluded(&path);
    FileChange {
        path,
        before,
        after: None,
        excluded,
        degraded: Some(format!("{reason}:{}", e.kind())),
        line_ranges: None,
        deleted_at: Vec::new(),
    }
}
fn change_event(
    kind: &str,
    change: FileChange,
    hook: &str,
    repo: &str,
    session: &str,
    tool: Option<&str>,
    after_incomplete: bool,
) -> Event {
    let mut e = Event::new(kind, Some(hook), Some(repo), Some(session), tool);
    e.path = Some(change.path);
    e.before_sha = change.before;
    e.after_sha = change.after;
    e.line_ranges = if change.excluded || change.degraded.is_some() {
        None
    } else {
        change.line_ranges
    };
    e.deleted_at = Some(change.deleted_at);
    e.excluded = Some(change.excluded);
    e.reason = change.degraded;
    e.after_incomplete = Some(after_incomplete);
    e
}
fn append_incomplete(path: &Path, repo: &str, hook: &str, session: &str, tool: Option<&str>) {
    let mut e = Event::new(
        "scan_incomplete",
        Some(hook),
        Some(repo),
        Some(session),
        tool,
    );
    e.degraded = Some(true);
    e.reason = Some("scan_budget".into());
    append_line(path, &e);
}
fn is_excluded(path: &str) -> bool {
    exclusion::classify_path(path).is_some()
}
fn expire_state(state: &mut RepoState) {
    let now = now_ns();
    state
        .windows
        .retain(|w| now.saturating_sub(w.opened_at) <= MAX_WINDOW_AGE_NS);
    state
        .recent_keys
        .retain(|k| now.saturating_sub(k.timestamp) <= MAX_WINDOW_AGE_NS);
}
fn cap_state(state: &mut RepoState) {
    if state.recent_keys.len() > MAX_RECENT_KEYS {
        let drop = state.recent_keys.len() - MAX_RECENT_KEYS;
        state.recent_keys.drain(0..drop);
    }
}
fn pending_external_flag(state: &mut RepoState, incomplete: bool) -> bool {
    let was_pending = state.scan_incomplete_pending;
    if incomplete {
        state.scan_incomplete_pending = true;
    } else if was_pending {
        state.scan_incomplete_pending = false;
    }
    was_pending
}
fn append_marker(ctx: &MarkerContext, event: &Event) {
    if let Some(id) = &ctx.repo_id {
        let path = ctx.home.join("repos").join(id).join("markers.jsonl");
        append_line(&path, event);
    } else {
        append_line(&ctx.home.join("inbox.jsonl"), event);
    }
}
fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Option<T> {
    fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
}
fn atomic_json<T: Serialize>(path: &Path, value: &T, sync: bool) -> std::io::Result<()> {
    let start = Instant::now();
    atomic_bytes(
        path,
        &serde_json::to_vec(value).map_err(std::io::Error::other)?,
        sync,
    )
    .inspect(|_| add_ns(&TIMINGS.writes_ns, start.elapsed()))
}
fn write_once(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let start = Instant::now();
    if path.exists() {
        let expected = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        if read_blob(path, expected).is_some() {
            add_ns(&TIMINGS.writes_ns, start.elapsed());
            return Ok(());
        }
    }
    let result = atomic_bytes(path, bytes, false);
    add_ns(&TIMINGS.writes_ns, start.elapsed());
    if result.is_err()
        && fs::metadata(path)
            .is_ok_and(|metadata| metadata.is_file() && metadata.len() == bytes.len() as u64)
    {
        return Ok(());
    }
    result
}
fn read_blob(path: &Path, expected_sha: &str) -> Option<Vec<u8>> {
    let bytes = fs::read(path).ok()?;
    (hash(&bytes) == expected_sha).then_some(bytes)
}
fn atomic_bytes(path: &Path, bytes: &[u8], sync: bool) -> std::io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("no parent"))?;
    ensure_private_dir(parent)?;
    let temp = parent.join(format!(".tmp-{}-{}", std::process::id(), now_ns()));
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        f.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    f.write_all(bytes)?;
    f.flush()?;
    if sync {
        let sync_start = Instant::now();
        f.sync_all()?;
        add_ns(&TIMINGS.sync_ns, sync_start.elapsed());
    }
    drop(f);
    #[cfg(windows)]
    if path.exists() {
        use std::os::windows::ffi::OsStrExt;
        let source: Vec<u16> = temp.as_os_str().encode_wide().chain(Some(0)).collect();
        let destination: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        // ReplaceFile preserves an atomic replacement for existing state/cache files.
        #[link(name = "Kernel32")]
        unsafe extern "system" {
            fn ReplaceFileW(
                replaced: *const u16,
                replacement: *const u16,
                backup: *const u16,
                flags: u32,
                exclude: *mut std::ffi::c_void,
                reserved: *mut std::ffi::c_void,
            ) -> i32;
        }
        let replaced = unsafe {
            ReplaceFileW(
                destination.as_ptr(),
                source.as_ptr(),
                std::ptr::null(),
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if replaced != 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        let _ = fs::remove_file(&temp);
        return Err(error);
    }
    match fs::rename(&temp, path) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = fs::remove_file(&temp);
            Err(error)
        }
    }
}
fn append_line(path: &Path, event: &Event) -> bool {
    append_events(path, std::slice::from_ref(event))
}
fn append_events(path: &Path, events: &[Event]) -> bool {
    let start = Instant::now();
    let mut bytes = Vec::new();
    for event in events {
        let Ok(mut line) = serde_json::to_vec(event) else {
            continue;
        };
        line.push(b'\n');
        bytes.extend_from_slice(&line);
    }
    if bytes.is_empty() {
        add_ns(&TIMINGS.log_work_ns, start.elapsed());
        return false;
    }
    if let Some(parent) = path.parent() {
        if ensure_private_dir(parent).is_err() {
            return false;
        }
    }
    let Ok(mut f) = OpenOptions::new()
        .create(true)
        .read(true)
        .append(true)
        .open(path)
    else {
        return false;
    };
    let mut last = [0];
    let torn = f
        .seek(SeekFrom::End(-1))
        .ok()
        .and_then(|_| f.read_exact(&mut last).ok())
        .is_some()
        && last[0] != b'\n';
    if torn && f.write_all(b"\n").is_err() {
        add_ns(&TIMINGS.log_work_ns, start.elapsed());
        return false;
    }
    let written = f.write_all(&bytes).is_ok();
    add_ns(&TIMINGS.log_work_ns, start.elapsed());
    written
}

fn trigger_auto_derive(repo_root: &Path) {
    if std::env::var_os("PROV_NO_AUTODERIVE").as_deref() == Some(std::ffi::OsStr::new("1")) {
        return;
    }
    let Ok(executable) = std::env::current_exe() else {
        return;
    };
    spawn_detached_pending_derive(&executable, repo_root);
}

pub fn spawn_pending_derive(repo_root: &Path) {
    trigger_auto_derive(repo_root);
}

fn spawn_detached_pending_derive(executable: &Path, repo_root: &Path) {
    let mut command = Command::new(executable);
    command
        .args(["derive", "--pending", "--repo"])
        .arg(repo_root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    configure_process_command(&mut command, true);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let _ = command.spawn();
}
#[cfg(test)]
fn complete_line_count(data: &[u8]) -> usize {
    let end = data.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
    data[..end]
        .split(|b| *b == b'\n')
        .filter(|l| !l.is_empty())
        .count()
}
fn ensure_private_dir(path: &Path) -> std::io::Result<()> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}
fn store_home() -> Option<PathBuf> {
    if let Some(v) = std::env::var_os("PROV_HOME") {
        return Some(PathBuf::from(v));
    }
    #[cfg(windows)]
    {
        std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .map(|p| p.join("prov"))
    }
    #[cfg(target_os = "macos")]
    {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .map(|p| p.join("Library/Application Support/prov"))
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".local/share")))
            .map(|p| p.join("prov"))
    }
}
fn raw_path(raw: &[u8]) -> PathBuf {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        PathBuf::from(std::ffi::OsStr::from_bytes(raw))
    }
    #[cfg(not(unix))]
    {
        PathBuf::from(String::from_utf8_lossy(raw).as_ref())
    }
}
fn clean_id(value: &str) -> String {
    let s: String = value
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(120)
        .collect();
    if s.is_empty() {
        "unknown".into()
    } else {
        s
    }
}
fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn now_ns() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

fn normalized(bytes: &[u8]) -> Option<String> {
    std::str::from_utf8(bytes)
        .ok()
        .map(|s| s.replace("\r\n", "\n"))
}
pub fn diff_changes_and_deletions(before: &[u8], after: &[u8]) -> (Vec<[usize; 2]>, Vec<usize>) {
    let (Some(old), Some(new)) = (normalized(before), normalized(after)) else {
        return (Vec::new(), Vec::new());
    };
    let diff = TextDiff::from_lines(&old, &new);
    let mut new_line = 0usize;
    let mut changed = Vec::new();
    let mut deleted = Vec::new();
    for item in diff.iter_all_changes() {
        match item.tag() {
            ChangeTag::Equal => {
                new_line += 1;
            }
            ChangeTag::Insert => {
                new_line += 1;
                changed.push(new_line);
            }
            ChangeTag::Delete => {
                deleted.push(new_line);
            }
        }
    }
    deleted.sort_unstable();
    deleted.dedup();
    (ranges(changed), deleted)
}
pub fn changed_line_ranges(before: &[u8], after: &[u8]) -> Vec<[usize; 2]> {
    diff_changes_and_deletions(before, after).0
}
pub fn deleted_positions(before: &[u8], after: &[u8]) -> Vec<usize> {
    diff_changes_and_deletions(before, after).1
}
fn ranges(mut lines: Vec<usize>) -> Vec<[usize; 2]> {
    lines.sort_unstable();
    lines.dedup();
    let mut out: Vec<[usize; 2]> = Vec::new();
    for line in lines {
        if let Some(last) = out.last_mut() {
            if line <= last[1] + 1 {
                last[1] = line;
                continue;
            }
        }
        out.push([line, line]);
    }
    out
}

#[derive(Clone)]
pub struct Watchdog(Arc<AtomicBool>);
impl Watchdog {
    pub fn finish(self) -> bool {
        self.0
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }
}
static MARKER_CONTEXT: Mutex<Option<MarkerContext>> = Mutex::new(None);
fn set_marker_context(ctx: &MarkerContext) {
    if let Ok(mut slot) = MARKER_CONTEXT.lock() {
        *slot = Some(ctx.clone());
    }
}
pub fn start_watchdog(cap: Duration) -> Watchdog {
    start_watchdog_with_output(cap, &[])
}

pub fn start_watchdog_with_output(cap: Duration, timeout_output: &'static [u8]) -> Watchdog {
    let done = Arc::new(AtomicBool::new(false));
    let d = done.clone();
    std::thread::spawn(move || {
        std::thread::sleep(cap);
        if d.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            if let Some(ctx) = MARKER_CONTEXT.lock().ok().and_then(|x| x.clone()) {
                let mut e = Event::new(
                    "time_cap",
                    ctx.hook_event_name.as_deref(),
                    ctx.repo_id.as_deref(),
                    ctx.session_id.as_deref(),
                    ctx.tool_use_id.as_deref(),
                );
                e.degraded = Some(true);
                e.reason = Some("time_cap".into());
                append_marker(&ctx, &e);
            }
            if !timeout_output.is_empty() {
                let mut stdout = std::io::stdout();
                let _ = stdout.write_all(timeout_output);
                let _ = stdout.flush();
            }
            std::process::exit(0);
        }
    });
    Watchdog(done)
}

pub fn process_json_timed(input: &[u8]) -> TimingSnapshot {
    TIMINGS.reset();
    let start = Instant::now();
    process_json(input);
    TIMINGS.total_ns.store(
        start.elapsed().as_nanos().min(u64::MAX as u128) as u64,
        Ordering::Relaxed,
    );
    TIMINGS.snapshot()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn process_command_flags_hide_windows_without_detaching() {
        assert_eq!(windows_process_creation_flags(false), 0x0800_0000);
        assert_eq!(
            windows_process_creation_flags(true),
            0x0800_0000 | 0x0000_0200
        );
        assert_eq!(windows_process_creation_flags(true) & 0x0000_0008, 0);
    }

    #[test]
    fn status_parser_handles_spaces_untracked_and_renames() {
        let mut b = b"? new file\0".to_vec();
        b.extend_from_slice(b"1 .M N... 100644 100644 100644 abcdef abcdef tracked file\0");
        let parsed = parse_status(&b);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].path, "new file");
        assert_eq!(parsed[1].path, "tracked file");
    }
    #[test]
    fn reader_skips_torn_last_line() {
        assert_eq!(complete_line_count(b"{\"ok\":1}\n{torn"), 1);
    }
    #[test]
    fn crlf_normalization_has_empty_ranges() {
        assert!(changed_line_ranges(b"alpha\n", b"alpha\r\n").is_empty());
    }
    #[test]
    fn diff_line_ranges_and_deleted_positions_match_new_file_lines() {
        type DiffCase<'a> = (&'a [u8], &'a [u8], &'a [[usize; 2]], &'a [usize]);
        let cases: &[DiffCase<'_>] = &[
            (b"a\nb\nc\n", b"a\nc\n", &[], &[1]),
            (b"a\nb\n", b"a\n", &[], &[1]),
            (b"a\nb\nc\n", b"b\nc\n", &[], &[0]),
            (b"a\nb\nc\n", b"a\nx\nc\n", &[[2, 2]], &[1]),
            (b"a\nb\n", b"a\nx\nb\n", &[[2, 2]], &[]),
            (
                b"a\nb\nc\nd\ne\n",
                b"x\nb\nd\ny\n",
                &[[1, 1], [4, 4]],
                &[0, 2, 3],
            ),
        ];
        for (before, after, ranges_expected, deleted_expected) in cases {
            let ranges = changed_line_ranges(before, after);
            assert_eq!(&ranges, ranges_expected);
            let line_count = normalized(after).unwrap().lines().count();
            assert!(ranges.iter().all(|r| r[1] <= line_count));
            assert_eq!(deleted_positions(before, after), *deleted_expected);
        }
    }
    #[test]
    fn combined_diff_computes_both_ranges_and_deletions_in_one_pass() {
        let before = b"line1\nline2\nline3\nline4\nline5\n";
        let after = b"line1\ninserted\nline2\nline4_modified\n";
        let (ranges, deleted) = diff_changes_and_deletions(before, after);
        assert_eq!(ranges, changed_line_ranges(before, after));
        assert_eq!(deleted, deleted_positions(before, after));
        assert_eq!(ranges, vec![[2, 2], [4, 4]]);
        assert_eq!(deleted, vec![3]);
    }
    #[test]
    fn exclusion_rules_match_expected_paths() {
        for p in [".env", ".env.local", "x.pem", "id_ed25519", "mysecret.txt"] {
            assert!(is_excluded(p));
        }
    }
    #[test]
    fn corrupt_content_addressed_blob_is_missing_and_repaired() {
        let dir =
            std::env::temp_dir().join(format!("prov-blob-{}-{}", std::process::id(), now_ns()));
        ensure_private_dir(&dir).unwrap();
        let expected = b"correct blob";
        let path = dir.join(hash(expected));
        fs::write(&path, b"corrupt blob").unwrap();
        assert!(read_blob(&path, path.file_name().unwrap().to_str().unwrap()).is_none());
        write_once(&path, expected).unwrap();
        assert_eq!(
            read_blob(&path, path.file_name().unwrap().to_str().unwrap()).unwrap(),
            expected
        );
        let _ = fs::remove_dir_all(dir);
    }
    #[test]
    fn incomplete_scan_flag_applies_once_to_next_external_window() {
        let mut state = RepoState::default();
        assert!(!pending_external_flag(&mut state, true));
        assert!(state.scan_incomplete_pending);
        assert!(pending_external_flag(&mut state, false));
        assert!(!state.scan_incomplete_pending);
        assert!(!pending_external_flag(&mut state, false));
    }

    #[test]
    fn missing_auto_derive_target_is_ignored_within_watchdog_budget() {
        let started = Instant::now();
        let missing = std::env::temp_dir()
            .join(format!("prov-missing-derive-{}", std::process::id()))
            .join("prov-hook");
        spawn_detached_pending_derive(&missing, Path::new("."));
        assert!(started.elapsed() < Duration::from_millis(1400));
    }

    #[cfg(windows)]
    #[test]
    fn replace_file_w_replaces_state_and_cleans_temp_file() {
        let dir =
            std::env::temp_dir().join(format!("prov-replace-{}-{}", std::process::id(), now_ns()));
        ensure_private_dir(&dir).unwrap();
        let file = dir.join("state.json");
        atomic_bytes(&file, b"old", true).unwrap();
        atomic_bytes(&file, b"new", true).unwrap();
        assert_eq!(fs::read(&file).unwrap(), b"new");
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
        let _ = fs::remove_dir_all(dir);
    }

    #[cfg(windows)]
    #[test]
    fn failed_replace_file_w_cleans_temp_file() {
        use std::os::windows::ffi::OsStrExt;
        let dir = std::env::temp_dir().join(format!(
            "prov-replace-fail-{}-{}",
            std::process::id(),
            now_ns()
        ));
        ensure_private_dir(&dir).unwrap();
        let file = dir.join("state.json");
        atomic_bytes(&file, b"old", true).unwrap();
        #[link(name = "Kernel32")]
        unsafe extern "system" {
            fn CreateFileW(
                name: *const u16,
                access: u32,
                share: u32,
                security: *mut std::ffi::c_void,
                disposition: u32,
                flags: u32,
                template: *mut std::ffi::c_void,
            ) -> *mut std::ffi::c_void;
            fn CloseHandle(handle: *mut std::ffi::c_void) -> i32;
        }
        let wide: Vec<u16> = file.as_os_str().encode_wide().chain(Some(0)).collect();
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                0x8000_0000,
                0,
                std::ptr::null_mut(),
                3,
                0x80,
                std::ptr::null_mut(),
            )
        };
        assert_ne!(handle as isize, -1);
        assert!(atomic_bytes(&file, b"new", true).is_err());
        unsafe {
            CloseHandle(handle);
        }
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
        let _ = fs::remove_dir_all(dir);
    }
}
