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

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct TimingSnapshot {
    pub total_ns: u64,
    pub git_processes: u64,
    pub repo_discovery_ns: u64,
    pub lock_ns: u64,
    pub git_status_ns: u64,
    pub hashing_snapshots_ns: u64,
    pub writes_ns: u64,
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
const LOCK_BUDGET: Duration = Duration::from_millis(250);
const MAX_RECENT_KEYS: usize = 1000;
const MAX_WINDOW_AGE_NS: u128 = 60 * 60 * 1_000_000_000;
const EXCLUSIONS: &[&str] = &[
    ".env*",
    "*.pem",
    "*.key",
    "*.pfx",
    "id_rsa*",
    "id_ed25519*",
    "*credentials*",
    "*secret*",
];

#[derive(Debug, Deserialize)]
struct HookInput {
    hook_event_name: Option<String>,
    session_id: Option<String>,
    turn_id: Option<String>,
    cwd: Option<String>,
    model: Option<String>,
    #[serde(rename = "tool_name")]
    _tool_name: Option<String>,
    tool_use_id: Option<String>,
    tool_input: Option<ToolInput>,
}
#[derive(Debug, Deserialize)]
struct ToolInput {
    command: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct FileState {
    size: u64,
    mtime_ns: u128,
    sha256: String,
}
#[derive(Debug, Default, Serialize, Deserialize)]
struct Shadow {
    files: BTreeMap<String, FileState>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct WindowState {
    session_id: String,
    tool_use_id: Option<String>,
    opened_at: u128,
    head: String,
    branch: String,
    overlap: bool,
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
    session_id: Option<String>,
    turn_id: Option<String>,
    tool_use_id: Option<String>,
    model: Option<String>,
    tool: Option<String>,
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
            session_id: session.map(str::to_owned),
            turn_id: None,
            tool_use_id: tool.map(str::to_owned),
            model: None,
            tool: None,
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
}
#[derive(Clone, Debug)]
struct FileChange {
    path: String,
    before: Option<String>,
    after: Option<String>,
    before_bytes: Option<Vec<u8>>,
    after_bytes: Option<Vec<u8>>,
    excluded: bool,
    degraded: Option<String>,
}

/// Contain all hook work. The executable wraps this in catch_unwind and has a process watchdog.
pub fn process_json(input: &[u8]) {
    let Ok(hook) = serde_json::from_slice::<HookInput>(input) else {
        return;
    };
    let (Some(event_name), Some(cwd)) = (hook.hook_event_name.as_deref(), hook.cwd.as_deref())
    else {
        return;
    };
    let cwd = Path::new(cwd);
    if !cwd.is_dir() {
        return;
    }
    let Some(home) = store_home() else { return };
    let session = clean_id(hook.session_id.as_deref().unwrap_or("unknown"));
    let tool_id = hook.tool_use_id.as_deref().map(str::to_owned);
    let discovery_start = Instant::now();
    let Some(mut repo) = repo_info(cwd, &home) else {
        add_ns(&TIMINGS.repo_discovery_ns, discovery_start.elapsed());
        return;
    };
    add_ns(&TIMINGS.repo_discovery_ns, discovery_start.elapsed());
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
    match event_name {
        "PreToolUse" => {
            let scan = timed_scan(&repo.root, &store.join("blobs"), &mut shadow, SCAN_BUDGET);
            let (changes, incomplete) = match scan {
                Ok(v) => v,
                Err(_) => (Vec::new(), true),
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
                opened_at: now,
                head: head.clone(),
                branch: branch.clone(),
                overlap,
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
            e.degraded = Some(incomplete);
            append_line(&event_path, &e);
            state.windows.push(window);
        }
        "PostToolUse" => {
            let scan = timed_scan(&repo.root, &store.join("blobs"), &mut shadow, SCAN_BUDGET);
            let (mut changes, incomplete) = match scan {
                Ok(v) => v,
                Err(_) => (Vec::new(), true),
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
            let opened = window.as_ref().map(|w| w.opened_at);
            let overlap = window.as_ref().is_some_and(|w| w.overlap)
                || state
                    .windows
                    .iter()
                    .any(|o| o.session_id != session && o.opened_at <= now_ns());
            let head_moved = window
                .as_ref()
                .is_some_and(|w| w.head != close_head || w.branch != close_branch);
            if head_moved {
                if let Some(window) = window.as_ref() {
                    let extra = head_move_changes(&repo.root, &window.head, &close_head, &changes);
                    changes.extend(extra);
                }
            }
            let command_sha = hook
                .tool_input
                .as_ref()
                .and_then(|v| v.command.as_deref())
                .map(|v| hash(v.as_bytes()));
            let command_length = hook
                .tool_input
                .as_ref()
                .and_then(|v| v.command.as_deref())
                .map(str::len);
            let now = now_ns();
            let mut event_batch = Vec::with_capacity(changes.len());
            for change in changes {
                let duplicate = tool_id.as_ref().is_some_and(|id| {
                    state.recent_keys.iter().any(|k| {
                        k.tool_use_id == *id && k.path == change.path && k.after_sha == change.after
                    })
                });
                if duplicate {
                    continue;
                }
                let kind = if head_moved {
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
                e.tool = Some("codex".into());
                e.command_sha256 = command_sha.clone();
                e.command_length = command_length;
                e.window_open = opened;
                e.window_close = Some(now);
                e.overlap = Some(overlap);
                e.head = Some(close_head.clone());
                e.branch = Some(close_branch.clone());
                if head_moved {
                    e.reason = Some("head_moved".into());
                    e.head_moved = Some(true);
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
            e.degraded = Some(incomplete);
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
    let _ = atomic_json(&state_path, &state);
    let _ = atomic_json(&shadow_path, &shadow);
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
    let repo_id = if let Some(cache) = cached {
        Some(cache.repo_id)
    } else {
        let mut roots: Vec<String> = git_text(
            &root_path,
            &["rev-list", "--first-parent", "--max-parents=0", "HEAD"],
        )?
        .lines()
        .map(str::to_owned)
        .collect();
        roots.sort();
        if roots.is_empty() {
            return None;
        }
        let id = hash(roots.join("\n").as_bytes());
        let _ = atomic_json(
            &cache_path,
            &RepoCache {
                repo_id: id.clone(),
            },
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

fn scan(
    root: &Path,
    blobs: &Path,
    shadow: &mut Shadow,
    budget: Duration,
) -> std::io::Result<(Vec<FileChange>, bool)> {
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
    let mut head_ids = Vec::new();
    for d in &dirty {
        if !d.untracked && !shadow.files.contains_key(&d.path) {
            if let Some(id) = &d.head_blob {
                head_ids.push(id.clone());
            }
        }
    }
    if start.elapsed() >= budget {
        return Ok((Vec::new(), true));
    }
    let head_contents = cat_blobs(root, &head_ids).unwrap_or_default();
    let mut changes = Vec::new();
    let mut incomplete = start.elapsed() > budget;
    for d in dirty {
        if start.elapsed() >= budget {
            incomplete = true;
            break;
        }
        let path = d.path.replace('\\', "/");
        let old_file = shadow
            .files
            .get(&path)
            .and_then(|s| fs::read(blobs.join(&s.sha256)).ok());
        if old_file.is_none()
            && d.head_blob.is_some()
            && !head_contents.contains_key(d.head_blob.as_ref().unwrap())
        {
            changes.push(FileChange {
                path: path.clone(),
                before: None,
                after: None,
                before_bytes: None,
                after_bytes: None,
                excluded: is_excluded(&path),
                degraded: Some("git_before_read:other".into()),
            });
            continue;
        }
        let (before_bytes, before_sha) = if let Some(bytes) = old_file {
            let sha = hash(&bytes);
            (Some(bytes), Some(sha))
        } else if let Some(id) = d.head_blob.as_ref().and_then(|id| head_contents.get(id)) {
            (Some(id.clone()), Some(hash(id)))
        } else {
            (Some(Vec::new()), None)
        };
        let full = root.join(raw_path(d.path.as_bytes()));
        let meta = match fs::symlink_metadata(&full) {
            Ok(m) if m.file_type().is_symlink() => {
                shadow.files.remove(&path);
                continue;
            }
            Ok(m) => Some(m),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => {
                changes.push(file_error(path, before_sha, "metadata", &e));
                continue;
            }
        };
        let mut after_bytes = None;
        if let Some(meta) = meta {
            if !meta.file_type().is_file() || meta.len() > MAX_FILE {
                shadow.files.remove(&path);
                continue;
            }
            let f = match File::open(&full) {
                Ok(f) => f,
                Err(e) => {
                    changes.push(file_error(path, before_sha, "open", &e));
                    continue;
                }
            };
            let mut bytes = Vec::with_capacity(meta.len() as usize);
            if let Err(e) = f.take(MAX_FILE + 1).read_to_end(&mut bytes) {
                changes.push(file_error(path, before_sha, "read", &e));
                continue;
            }
            if bytes.len() as u64 > MAX_FILE || bytes[..bytes.len().min(8192)].contains(&0) {
                shadow.files.remove(&path);
                continue;
            }
            after_bytes = Some(bytes);
        }
        let excluded = is_excluded(&path);
        let before_sha =
            before_sha.or_else(|| before_bytes.as_deref().filter(|b| !b.is_empty()).map(hash));
        let after_sha = after_bytes.as_deref().map(hash);
        if before_sha != after_sha {
            if !excluded {
                if let Some(bytes) = after_bytes.as_deref() {
                    if let Err(e) =
                        write_once(&blobs.join(after_sha.as_deref().unwrap_or("")), bytes)
                    {
                        changes.push(file_error(path, before_sha, "snapshot", &e));
                        continue;
                    }
                } else if let Some(bytes) = before_bytes.as_deref() {
                    let old_sha = hash(bytes);
                    if let Err(e) = write_once(&blobs.join(old_sha), bytes) {
                        changes.push(file_error(path, before_sha, "snapshot", &e));
                        continue;
                    }
                }
            }
            changes.push(FileChange {
                path: path.clone(),
                before: before_sha.clone(),
                after: after_sha.clone(),
                before_bytes: before_bytes.clone(),
                after_bytes: after_bytes.clone(),
                excluded,
                degraded: None,
            });
        }
        if excluded || after_bytes.is_none() {
            shadow.files.remove(&path);
        } else if let (Some(sha), Some(bytes)) = (after_sha, after_bytes) {
            let mtime_ns = fs::metadata(root.join(raw_path(d.path.as_bytes())))
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_nanos());
            shadow.files.insert(
                path,
                FileState {
                    size: bytes.len() as u64,
                    mtime_ns,
                    sha256: sha,
                },
            );
        }
    }
    Ok((changes, incomplete))
}

fn timed_scan(
    root: &Path,
    blobs: &Path,
    shadow: &mut Shadow,
    budget: Duration,
) -> std::io::Result<(Vec<FileChange>, bool)> {
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
            head_blob: if typ == b'2' { None } else { head_blob.clone() },
            untracked: false,
        });
        if typ == b'2' {
            if let Some(old) = records.next() {
                out.push(DirtyPath {
                    path: String::from_utf8_lossy(old).into_owned(),
                    head_blob,
                    untracked: false,
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

fn head_move_changes(
    root: &Path,
    old_head: &str,
    new_head: &str,
    existing: &[FileChange],
) -> Vec<FileChange> {
    let old = old_head.strip_prefix("detached:").unwrap_or(old_head);
    let new = new_head.strip_prefix("detached:").unwrap_or(new_head);
    if old == "unknown" || new == "unknown" {
        return Vec::new();
    }
    let Some(out) = git_output(
        root,
        &["diff", "--no-renames", "--name-only", "-z", old, new],
        None,
    )
    .ok() else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    let known: HashSet<&str> = existing.iter().map(|c| c.path.as_str()).collect();
    let mut changes = Vec::new();
    for raw in out.stdout.split(|b| *b == 0).filter(|p| !p.is_empty()) {
        let path = String::from_utf8_lossy(raw).into_owned();
        if known.contains(path.as_str()) {
            continue;
        }
        let old_arg = format!("{old}:{path}");
        let new_arg = format!("{new}:{path}");
        let before = git_output(root, &["show", old_arg.as_str()], None)
            .ok()
            .filter(|o| o.status.success())
            .map(|o| o.stdout);
        let after = git_output(root, &["show", new_arg.as_str()], None)
            .ok()
            .filter(|o| o.status.success())
            .map(|o| o.stdout);
        if before
            .as_ref()
            .is_some_and(|b| b.len() as u64 > MAX_FILE || b[..b.len().min(8192)].contains(&0))
            || after
                .as_ref()
                .is_some_and(|b| b.len() as u64 > MAX_FILE || b[..b.len().min(8192)].contains(&0))
        {
            continue;
        }
        let before_sha = before.as_deref().map(hash);
        let after_sha = after.as_deref().map(hash);
        if before_sha != after_sha {
            changes.push(FileChange {
                excluded: is_excluded(&path),
                path,
                before: before_sha,
                after: after_sha,
                before_bytes: before,
                after_bytes: after,
                degraded: None,
            });
        }
    }
    changes
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
        before_bytes: None,
        after_bytes: None,
        excluded,
        degraded: Some(format!("{reason}:{}", e.kind())),
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
    e.line_ranges = match (
        change.before_bytes.as_deref(),
        change.after_bytes.as_deref(),
    ) {
        (Some(a), Some(b)) => Some(changed_line_ranges(a, b)),
        (None, Some(b)) => Some(line_ranges(b)),
        _ => None,
    };
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
    let base = path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase();
    EXCLUSIONS.iter().any(|p| match *p {
        ".env*" => base.starts_with(".env"),
        "*.pem" => base.ends_with(".pem"),
        "*.key" => base.ends_with(".key"),
        "*.pfx" => base.ends_with(".pfx"),
        "id_rsa*" => base.starts_with("id_rsa"),
        "id_ed25519*" => base.starts_with("id_ed25519"),
        "*credentials*" => base.contains("credentials"),
        "*secret*" => base.contains("secret"),
        _ => false,
    })
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
fn atomic_json<T: Serialize>(path: &Path, value: &T) -> std::io::Result<()> {
    let start = Instant::now();
    atomic_bytes(
        path,
        &serde_json::to_vec(value).map_err(std::io::Error::other)?,
    )
    .inspect(|_| add_ns(&TIMINGS.writes_ns, start.elapsed()))
}
fn write_once(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let start = Instant::now();
    if path.exists() {
        add_ns(&TIMINGS.writes_ns, start.elapsed());
        return Ok(());
    }
    let result = atomic_bytes(path, bytes);
    add_ns(&TIMINGS.writes_ns, start.elapsed());
    result
}
fn atomic_bytes(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
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
    f.sync_all()?;
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
        return Err(std::io::Error::last_os_error());
    }
    match fs::rename(&temp, path) {
        Ok(()) => Ok(()),
        Err(_e) if path.exists() => {
            let _ = fs::remove_file(&temp);
            Ok(())
        }
        Err(e) => Err(e),
    }
}
fn append_line(path: &Path, event: &Event) {
    append_events(path, std::slice::from_ref(event));
}
fn append_events(path: &Path, events: &[Event]) {
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
        return;
    }
    if let Some(parent) = path.parent() {
        if ensure_private_dir(parent).is_err() {
            return;
        }
    }
    let Ok(mut f) = OpenOptions::new()
        .create(true)
        .read(true)
        .append(true)
        .open(path)
    else {
        return;
    };
    let mut last = [0];
    let torn = f
        .seek(SeekFrom::End(-1))
        .ok()
        .and_then(|_| f.read_exact(&mut last).ok())
        .is_some()
        && last[0] != b'\n';
    if torn {
        let _ = f.write_all(b"\n");
    }
    let _ = f.write_all(&bytes);
    add_ns(&TIMINGS.log_work_ns, start.elapsed());
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
fn line_ranges(bytes: &[u8]) -> Vec<[usize; 2]> {
    let Some(text) = normalized(bytes) else {
        return Vec::new();
    };
    let diff = TextDiff::from_lines("", &text);
    let mut changed = Vec::new();
    let mut line = 0usize;
    for op in diff.ops() {
        for item in diff.iter_changes(op) {
            match item.tag() {
                ChangeTag::Equal => line += 1,
                ChangeTag::Insert => {
                    line += 1;
                    changed.push(line);
                }
                ChangeTag::Delete => {}
            }
        }
    }
    ranges(changed)
}
fn changed_line_ranges(before: &[u8], after: &[u8]) -> Vec<[usize; 2]> {
    let (Some(old), Some(new)) = (normalized(before), normalized(after)) else {
        return Vec::new();
    };
    let diff = TextDiff::from_lines(&old, &new);
    let mut new_line = 0usize;
    let mut changed = Vec::new();
    for item in diff.iter_all_changes() {
        match item.tag() {
            ChangeTag::Equal => new_line += 1,
            ChangeTag::Insert => {
                new_line += 1;
                changed.push(new_line);
            }
            ChangeTag::Delete => changed.push((new_line + 1).max(1)),
        }
    }
    ranges(changed)
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
    pub fn finish(self) {
        self.0.store(true, Ordering::SeqCst);
    }
}
static MARKER_CONTEXT: Mutex<Option<MarkerContext>> = Mutex::new(None);
fn set_marker_context(ctx: &MarkerContext) {
    if let Ok(mut slot) = MARKER_CONTEXT.lock() {
        *slot = Some(ctx.clone());
    }
}
pub fn start_watchdog(cap: Duration) -> Watchdog {
    let done = Arc::new(AtomicBool::new(false));
    let d = done.clone();
    std::thread::spawn(move || {
        std::thread::sleep(cap);
        if !d.load(Ordering::SeqCst) {
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
    fn exclusion_rules_match_expected_paths() {
        for p in [".env", ".env.local", "x.pem", "id_ed25519", "mysecret.txt"] {
            assert!(is_excluded(p));
        }
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
}
