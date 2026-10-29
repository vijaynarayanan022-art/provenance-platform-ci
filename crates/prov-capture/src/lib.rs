use fd_lock::RwLock;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use similar::{ChangeTag, TextDiff};
use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const MAX_FILE: u64 = 1024 * 1024;
const SCAN_BUDGET: Duration = Duration::from_millis(1500);
const LOCK_BUDGET: Duration = Duration::from_millis(650);
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
type FileChange = (
    String,
    Option<String>,
    Option<String>,
    Option<Vec<u8>>,
    Option<Vec<u8>>,
);
type ScanResult = std::io::Result<(Vec<FileChange>, bool)>;

#[derive(Debug, Deserialize)]
struct HookInput {
    hook_event_name: Option<String>,
    session_id: Option<String>,
    turn_id: Option<String>,
    cwd: Option<String>,
    model: Option<String>,
    tool_name: Option<String>,
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
#[derive(Debug, Serialize, Deserialize)]
struct Shadow {
    files: BTreeMap<String, FileState>,
}
#[derive(Debug, Serialize)]
struct Event {
    schema_version: &'static str,
    event: String,
    timestamp: u128,
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,
    before_sha: Option<String>,
    after_sha: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    line_ranges: Option<Vec<[usize; 2]>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    turn_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_use_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    command_sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    window_open: Option<u128>,
    #[serde(skip_serializing_if = "Option::is_none")]
    window_close: Option<u128>,
    #[serde(skip_serializing_if = "Option::is_none")]
    overlap: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    degraded: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
}
impl Event {
    fn new(name: &str) -> Self {
        Self {
            schema_version: "prov.event/0.1",
            event: name.into(),
            timestamp: now_ns(),
            path: None,
            before_sha: None,
            after_sha: None,
            line_ranges: None,
            session_id: None,
            turn_id: None,
            tool_use_id: None,
            model: None,
            tool: None,
            command_sha256: None,
            window_open: None,
            window_close: None,
            overlap: None,
            degraded: None,
            reason: None,
        }
    }
}

/// Process one Codex hook payload. Errors are deliberately contained by the binary entrypoint.
pub fn process_json(input: &[u8]) {
    let Ok(hook) = serde_json::from_slice::<HookInput>(input) else {
        return;
    };
    let Some(cwd) = hook.cwd.as_deref() else {
        return;
    };
    let cwd = Path::new(cwd);
    if !cwd.is_dir() {
        return;
    }
    let Some(repo) = repo_info(cwd) else { return };
    let Some(home) = store_home() else { return };
    let repo_id = hash(repo.roots.join("\n").as_bytes());
    let store = home.join("repos").join(&repo_id);
    if ensure_private_dir(&store).is_err() {
        return;
    }
    let worktree_id = hash(repo.git_dir.to_string_lossy().as_bytes());
    let shadow_path = store.join("shadow").join(format!("{worktree_id}.json"));
    let session = clean_id(hook.session_id.as_deref().unwrap_or("unknown"));
    let event_path = store.join("events").join(format!("{session}.jsonl"));
    for dir in [
        store.join("blobs"),
        store.join("events"),
        store.join("shadow"),
    ] {
        if ensure_private_dir(&dir).is_err() {
            return;
        }
    }
    let lock_path = store.join("repo.lock");
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
                std::thread::sleep(Duration::from_millis(10))
            }
            Err(_) => break None,
        }
    };
    let Some(_guard) = guard else {
        let mut event = Event::new("lock_timeout");
        event.session_id = Some(session);
        append_line(&home.join("inbox.jsonl"), &event);
        return;
    };
    let Some(name) = hook.hook_event_name.as_deref() else {
        return;
    };
    let _reported_tool_name = hook.tool_name.as_deref();
    match name {
        "PreToolUse" => {
            let scan = scan(
                cwd,
                &repo.root,
                &store.join("blobs"),
                &shadow_path,
                SCAN_BUDGET,
            );
            let (changes, incomplete) = match scan {
                Ok(v) => v,
                Err(_) => (Vec::new(), true),
            };
            if incomplete {
                let mut e = Event::new("scan_incomplete");
                e.session_id = Some(session.clone());
                e.degraded = Some(true);
                append_line(&event_path, &e);
            }
            for (path, before, after, _, _) in changes {
                let mut e = Event::new("external_change");
                e.path = Some(path);
                e.before_sha = before;
                e.after_sha = after;
                e.session_id = Some(session.clone());
                e.degraded = Some(incomplete);
                append_line(&event_path, &e);
            }
            let mut e = Event::new("window_open");
            e.session_id = Some(session);
            e.tool_use_id = hook.tool_use_id;
            e.overlap = Some(other_window_active(
                &store.join("events"),
                e.session_id.as_deref(),
                now_ns(),
            ));
            e.degraded = Some(incomplete);
            append_line(&event_path, &e);
        }
        "PostToolUse" => {
            let scan = scan(
                cwd,
                &repo.root,
                &store.join("blobs"),
                &shadow_path,
                SCAN_BUDGET,
            );
            let (changes, incomplete) = match scan {
                Ok(v) => v,
                Err(_) => (Vec::new(), true),
            };
            if incomplete {
                let mut e = Event::new("scan_incomplete");
                e.session_id = Some(session.clone());
                e.degraded = Some(true);
                append_line(&event_path, &e);
            }
            let (opened, overlap) = find_window_info(
                &store.join("events"),
                &event_path,
                session.as_str(),
                hook.tool_use_id.as_deref(),
            );
            let closed = now_ns();
            let cmd_sha = hook
                .tool_input
                .as_ref()
                .and_then(|v| v.command.as_deref())
                .map(|v| hash(v.as_bytes()));
            let mut dedupe = existing_ai_keys(&event_path);
            for (path, before, after, before_bytes, bytes) in changes {
                if let Some(tool_id) = hook.tool_use_id.as_deref() {
                    if !dedupe.insert((tool_id.to_owned(), path.clone(), after.clone())) {
                        continue;
                    }
                }
                let mut e = Event::new("ai_change");
                e.path = Some(path);
                e.before_sha = before;
                e.after_sha = after;
                e.line_ranges = match (before_bytes.as_deref(), bytes.as_deref()) {
                    (Some(old), Some(new)) => Some(changed_line_ranges(old, new)),
                    (None, Some(new)) => Some(line_ranges(new)),
                    (_, None) => None,
                };
                e.session_id = Some(session.clone());
                e.turn_id = hook.turn_id.clone();
                e.tool_use_id = hook.tool_use_id.clone();
                e.model = hook.model.clone();
                e.tool = Some("codex".into());
                e.command_sha256 = cmd_sha.clone();
                e.window_open = opened;
                e.window_close = Some(closed);
                e.overlap = Some(overlap);
                e.degraded = Some(incomplete);
                append_line(&event_path, &e);
            }
            let mut e = Event::new("window_close");
            e.session_id = Some(session);
            e.tool_use_id = hook.tool_use_id;
            e.window_open = opened;
            e.window_close = Some(closed);
            e.overlap = Some(overlap);
            e.degraded = Some(incomplete);
            append_line(&event_path, &e);
        }
        "Stop" => {
            let mut e = Event::new("turn_end");
            e.session_id = Some(session);
            e.turn_id = hook.turn_id;
            append_line(&event_path, &e);
        }
        _ => {}
    }
}

struct RepoInfo {
    root: PathBuf,
    git_dir: PathBuf,
    roots: Vec<String>,
}
fn repo_info(cwd: &Path) -> Option<RepoInfo> {
    let root = git(cwd, &["rev-parse", "--show-toplevel"])?
        .trim()
        .to_owned();
    let git_dir = git(Path::new(&root), &["rev-parse", "--absolute-git-dir"])?
        .trim()
        .to_owned();
    let mut roots: Vec<_> = git(Path::new(&root), &["rev-list", "--max-parents=0", "--all"])?
        .lines()
        .map(str::to_owned)
        .collect();
    roots.sort();
    if roots.is_empty() {
        return None;
    }
    Some(RepoInfo {
        root: PathBuf::from(root),
        git_dir: PathBuf::from(git_dir),
        roots,
    })
}
fn git(cwd: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-c")
        .arg("core.quotepath=false")
        .args(args)
        .current_dir(cwd)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

fn scan(cwd: &Path, root: &Path, blobs: &Path, shadow_path: &Path, budget: Duration) -> ScanResult {
    let start = Instant::now();
    let listing = Command::new("git")
        .args([
            "-c",
            "core.quotepath=false",
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ])
        .current_dir(root)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()?;
    if !listing.status.success() {
        return Err(std::io::Error::other("git ls-files failed"));
    }
    let old: Shadow = fs::read(shadow_path)
        .ok()
        .and_then(|v| serde_json::from_slice(&v).ok())
        .unwrap_or(Shadow {
            files: BTreeMap::new(),
        });
    let mut state = old.files.clone();
    let mut seen = HashSet::new();
    let mut changes = Vec::new();
    let mut incomplete = false;
    for raw in listing.stdout.split(|b| *b == 0).filter(|p| !p.is_empty()) {
        if start.elapsed() >= budget {
            incomplete = true;
            break;
        }
        let path = String::from_utf8_lossy(raw).replace('\\', "/");
        if excluded(&path) {
            state.remove(&path);
            seen.insert(path);
            continue;
        }
        let full = root.join(raw_path(raw));
        seen.insert(path.clone());
        let Ok(meta) = fs::symlink_metadata(&full) else {
            if let Some(prev) = old.files.get(&path) {
                changes.push((
                    path.clone(),
                    Some(prev.sha256.clone()),
                    None,
                    fs::read(blobs.join(&prev.sha256)).ok(),
                    None,
                ));
                state.remove(&path);
            }
            continue;
        };
        if !meta.file_type().is_file() || meta.len() > MAX_FILE {
            state.remove(&path);
            continue;
        }
        let modified = meta
            .modified()
            .unwrap_or(UNIX_EPOCH)
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let mtime_ns = modified.as_nanos();
        if old
            .files
            .get(&path)
            .is_some_and(|s| s.size == meta.len() && s.mtime_ns == mtime_ns)
        {
            continue;
        }
        let f = match File::open(&full) {
            Ok(f) => f,
            Err(_) => continue,
        };
        let mut bytes = Vec::with_capacity(meta.len() as usize);
        f.take(MAX_FILE + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_FILE || bytes[..bytes.len().min(8192)].contains(&0) {
            state.remove(&path);
            continue;
        }
        let digest = hash(&bytes);
        let before = old.files.get(&path).map(|s| s.sha256.clone());
        if before.as_deref() != Some(&digest) {
            write_once(&blobs.join(&digest), &bytes)?;
            let before_bytes = before
                .as_ref()
                .and_then(|sha| fs::read(blobs.join(sha)).ok());
            changes.push((
                path.clone(),
                before,
                Some(digest.clone()),
                before_bytes,
                Some(bytes.clone()),
            ));
        }
        state.insert(
            path,
            FileState {
                size: bytes.len() as u64,
                mtime_ns,
                sha256: digest,
            },
        );
    }
    if !incomplete {
        for (path, prev) in &old.files {
            if !seen.contains(path) {
                changes.push((
                    path.clone(),
                    Some(prev.sha256.clone()),
                    None,
                    fs::read(blobs.join(&prev.sha256)).ok(),
                    None,
                ));
                state.remove(path);
            }
        }
    }
    atomic_json(shadow_path, &Shadow { files: state })?;
    let _ = cwd;
    Ok((changes, incomplete))
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
fn excluded(path: &str) -> bool {
    let base = path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase();
    EXCLUSIONS.iter().any(|pattern| match *pattern {
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
fn all_events(dir: &Path) -> Vec<serde_json::Value> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        if !entry.path().extension().is_some_and(|e| e == "jsonl") {
            continue;
        }
        let Ok(data) = fs::read(entry.path()) else {
            continue;
        };
        for line in complete_lines(&data) {
            if let Ok(v) = serde_json::from_slice::<serde_json::Value>(line) {
                out.push(v);
            }
        }
    }
    out
}
fn event_key(v: &serde_json::Value) -> (String, String) {
    (
        v.get("session_id")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_owned(),
        v.get("tool_use_id")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_owned(),
    )
}
fn other_window_active(dir: &Path, session: Option<&str>, at: u128) -> bool {
    windows(dir).iter().any(|((s, _), (opened, closed))| {
        Some(s.as_str()) != session && *opened <= at && closed.is_none_or(|close| close > at)
    })
}
fn find_window_info(
    dir: &Path,
    _path: &Path,
    session: &str,
    tool: Option<&str>,
) -> (Option<u128>, bool) {
    let events = all_events(dir);
    let key = (session.to_owned(), tool.unwrap_or("").to_owned());
    let mut opened = None;
    let mut own_overlap = false;
    for v in &events {
        if event_key(v) == key && v.get("event").and_then(|x| x.as_str()) == Some("window_open") {
            opened = v.get("timestamp").and_then(|x| x.as_u64()).map(u128::from);
            own_overlap = v.get("overlap").and_then(|x| x.as_bool()).unwrap_or(false);
        }
    }
    own_overlap |= opened.is_some_and(|start| {
        windows(dir)
            .iter()
            .any(|((s, _), (other_start, other_end))| {
                if s == session {
                    return false;
                }
                let end = other_end.unwrap_or_else(now_ns);
                *other_start <= now_ns() && end >= start
            })
    });
    (opened, own_overlap)
}
fn windows(dir: &Path) -> BTreeMap<(String, String), (u128, Option<u128>)> {
    let mut found = BTreeMap::new();
    let mut events = all_events(dir);
    events.sort_by_key(|v| v.get("timestamp").and_then(|x| x.as_u64()).unwrap_or(0));
    for v in events {
        let ts = u128::from(v.get("timestamp").and_then(|x| x.as_u64()).unwrap_or(0));
        let key = event_key(&v);
        match v.get("event").and_then(|x| x.as_str()).unwrap_or("") {
            "window_open" => {
                found.insert(key, (ts, None));
            }
            "window_close" => {
                if let Some(window) = found.get_mut(&key) {
                    window.1 = Some(ts);
                }
            }
            _ => {}
        }
    }
    found
}
fn existing_ai_keys(path: &Path) -> HashSet<(String, String, Option<String>)> {
    let Ok(data) = fs::read(path) else {
        return HashSet::new();
    };
    complete_lines(&data)
        .filter_map(|line| serde_json::from_slice::<serde_json::Value>(line).ok())
        .filter(|v| v.get("event").and_then(|x| x.as_str()) == Some("ai_change"))
        .map(|v| {
            (
                v.get("tool_use_id")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_owned(),
                v.get("path")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_owned(),
                v.get("after_sha")
                    .and_then(|x| x.as_str())
                    .map(str::to_owned),
            )
        })
        .collect()
}
fn complete_lines(data: &[u8]) -> impl Iterator<Item = &[u8]> {
    let end = data
        .iter()
        .rposition(|b| *b == b'\n')
        .map(|i| i + 1)
        .unwrap_or(0);
    data[..end].split(|b| *b == b'\n').filter(|l| !l.is_empty())
}
fn line_ranges(bytes: &[u8]) -> Vec<[usize; 2]> {
    let Ok(new) = std::str::from_utf8(bytes) else {
        return Vec::new();
    };
    let normalized = new.replace("\r\n", "\n");
    let diff = TextDiff::from_lines("", normalized.as_str());
    let mut lines = Vec::new();
    let mut n = 0usize;
    for op in diff.ops() {
        for change in diff.iter_changes(op) {
            match change.tag() {
                ChangeTag::Equal => n += 1,
                ChangeTag::Insert => {
                    n += 1;
                    lines.push(n);
                }
                ChangeTag::Delete => {}
            }
        }
    }
    ranges(lines)
}
fn changed_line_ranges(old: &[u8], new: &[u8]) -> Vec<[usize; 2]> {
    let (Ok(old), Ok(new)) = (std::str::from_utf8(old), std::str::from_utf8(new)) else {
        return Vec::new();
    };
    let old = old.replace("\r\n", "\n");
    let new = new.replace("\r\n", "\n");
    let diff = TextDiff::from_lines(old.as_str(), new.as_str());
    let mut old_line = 0usize;
    let mut new_line = 0usize;
    let mut changed = Vec::new();
    for change in diff.iter_all_changes() {
        match change.tag() {
            ChangeTag::Equal => {
                old_line += 1;
                new_line += 1;
            }
            ChangeTag::Delete => old_line += 1,
            ChangeTag::Insert => {
                new_line += 1;
                changed.push(new_line);
            }
        }
    }
    let _ = old_line;
    ranges(changed)
}
fn ranges(lines: Vec<usize>) -> Vec<[usize; 2]> {
    let mut out: Vec<[usize; 2]> = Vec::new();
    for n in lines {
        if let Some(last) = out.last_mut() {
            if n == last[1] + 1 {
                last[1] = n;
                continue;
            }
        }
        out.push([n, n]);
    }
    out
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
fn ensure_private_dir(path: &Path) -> std::io::Result<()> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}
fn write_once(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if path.exists() {
        return Ok(());
    }
    atomic_bytes(path, bytes)
}
fn atomic_json<T: Serialize>(path: &Path, value: &T) -> std::io::Result<()> {
    atomic_bytes(
        path,
        &serde_json::to_vec(value).map_err(std::io::Error::other)?,
    )
}
fn atomic_bytes(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("no parent"))?;
    ensure_private_dir(parent)?;
    let temp = parent.join(format!(".tmp-{}-{}", std::process::id(), now_ns()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(bytes)?;
    file.flush()?;
    file.sync_all()?;
    drop(file);
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
    let Ok(mut bytes) = serde_json::to_vec(event) else {
        return;
    };
    bytes.push(b'\n');
    if let Some(parent) = path.parent() {
        if ensure_private_dir(parent).is_err() {
            return;
        }
    }
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = f.write(&bytes);
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
fn clean_id(value: &str) -> String {
    let safe: String = value
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(120)
        .collect();
    if safe.is_empty() {
        "unknown".into()
    } else {
        safe
    }
}

/// Read hook stdin through this helper to cap command work externally with a watchdog.
pub fn run_with_cap(input: Vec<u8>, cap: Duration) {
    let watchdog = start_watchdog(cap);
    process_json(&input);
    watchdog.finish();
}

/// Start the process-wide deadline before stdin is read.
pub struct Watchdog(Arc<AtomicBool>);
impl Watchdog {
    pub fn finish(self) {
        self.0.store(true, Ordering::SeqCst);
    }
}
pub fn start_watchdog(cap: Duration) -> Watchdog {
    let done = Arc::new(AtomicBool::new(false));
    let finished = Arc::clone(&done);
    std::thread::spawn(move || {
        std::thread::sleep(cap);
        if !finished.load(Ordering::SeqCst) {
            if let Some(home) = store_home() {
                let mut e = Event::new("degraded");
                e.degraded = Some(true);
                e.reason = Some("time_cap".into());
                append_line(&home.join("inbox.jsonl"), &e);
            }
            std::process::exit(0);
        }
    });
    Watchdog(done)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reader_skips_torn_line() {
        assert_eq!(complete_lines(b"{\"ok\":1}\n{torn").count(), 1);
    }
    #[test]
    fn crlf_normalization_has_empty_ranges() {
        assert!(changed_line_ranges(b"alpha\n", b"alpha\r\n").is_empty());
    }
    #[test]
    fn exclusion_rules_cover_secrets() {
        for v in [".env", ".env.local", "x.pem", "id_ed25519", "mysecret.txt"] {
            assert!(excluded(v));
        }
    }
}
