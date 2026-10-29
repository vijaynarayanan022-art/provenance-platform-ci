use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

struct Fixture {
    root: PathBuf,
    repo: PathBuf,
    home: PathBuf,
}
impl Fixture {
    fn new(name: &str) -> Self {
        let base =
            std::env::temp_dir().join(format!("prov-{name}-{}-{}", std::process::id(), now()));
        let repo = base.join("repository with spaces ü");
        let home = base.join("store");
        fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init"]);
        git(&repo, &["config", "user.email", "test@example.invalid"]);
        git(&repo, &["config", "user.name", "Test"]);
        fs::write(repo.join("tracked.txt"), "one\ntwo\n").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-m", "initial"]);
        Self {
            root: base,
            repo,
            home,
        }
    }
    fn hook(
        &self,
        event: &str,
        session: &str,
        tool_id: &str,
        command: Option<&str>,
    ) -> (i32, Vec<u8>, Vec<u8>) {
        self.hook_at(&self.repo, &self.home, event, session, tool_id, command)
    }
    fn hook_at(
        &self,
        cwd: &Path,
        home: &Path,
        event: &str,
        session: &str,
        tool_id: &str,
        command: Option<&str>,
    ) -> (i32, Vec<u8>, Vec<u8>) {
        let payload = json!({ "hook_event_name": event, "session_id": session, "turn_id": "turn-1", "cwd": cwd,
            "model": "test-model", "tool_name": "PowerShell", "tool_use_id": tool_id,
            "tool_input": { "command": command }, "transcript_path": "IGNORE_ME", "last_assistant_message": "IGNORE_ME" });
        let mut child = Command::new(env!("CARGO_BIN_EXE_prov-hook"))
            .arg("codex")
            .env("PROV_HOME", home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(payload.to_string().as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        (out.status.code().unwrap_or(-1), out.stdout, out.stderr)
    }
    fn events(&self) -> Vec<Value> {
        events_in(&self.home)
    }
    fn drop_root(&self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}
fn now() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}
fn git(cwd: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?} failed");
}
fn git_text(cwd: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .unwrap();
    assert!(out.status.success());
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}
fn events_in(home: &Path) -> Vec<Value> {
    let mut out = Vec::new();
    let mut todo = vec![home.to_path_buf()];
    while let Some(dir) = todo.pop() {
        let Ok(entries) = fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                todo.push(path);
            } else if path.extension().is_some_and(|e| e == "jsonl")
                && path.file_name().is_some_and(|f| f != "inbox.jsonl")
            {
                let bytes = fs::read(path).unwrap();
                let end = bytes.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
                for line in bytes[..end]
                    .split(|b| *b == b'\n')
                    .filter(|l| !l.is_empty())
                {
                    out.push(serde_json::from_slice(line).unwrap());
                }
            }
        }
    }
    out
}
fn sha(s: &[u8]) -> String {
    format!("{:x}", Sha256::digest(s))
}
fn select<'a>(events: &'a [Value], event: &str) -> Vec<&'a Value> {
    events.iter().filter(|e| e["event"] == event).collect()
}

#[test]
fn malformed_stdin_is_silent_and_successful() {
    let exe = env!("CARGO_BIN_EXE_prov-hook");
    for input in [
        Vec::new(),
        b"garbage".to_vec(),
        vec![b' '; 2 * 1024 * 1024],
        br#"{"cwd":[]}"#.to_vec(),
    ] {
        let mut child = Command::new(exe)
            .arg("codex")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(&input).unwrap();
        let out = child.wait_with_output().unwrap();
        assert_eq!(out.status.code(), Some(0));
        assert!(out.stdout.is_empty());
        assert!(out.stderr.is_empty());
    }
}

#[test]
fn invalid_and_missing_cwd_do_not_write() {
    let f = Fixture::new("badcwd");
    for cwd in [f.root.join("missing"), f.root.join("not-repo")] {
        if cwd.ends_with("not-repo") {
            fs::create_dir_all(&cwd).unwrap();
        }
        let (code, out, err) = f.hook_at(&cwd, &f.home, "PreToolUse", "s", "t", None);
        assert_eq!(code, 0);
        assert!(out.is_empty() && err.is_empty());
    }
    assert!(!f.home.exists());
    f.drop_root();
}

#[test]
fn missing_store_folder_is_silent() {
    let f = Fixture::new("badstore");
    fs::write(f.root.join("store-is-file"), "not a directory").unwrap();
    let (code, out, err) = f.hook_at(
        &f.repo,
        &f.root.join("store-is-file"),
        "Stop",
        "s",
        "t",
        None,
    );
    assert_eq!(code, 0);
    assert!(out.is_empty() && err.is_empty());
    f.drop_root();
}

#[test]
fn ai_change_has_hashes_ranges_and_command_digest() {
    let f = Fixture::new("ai");
    f.hook("PreToolUse", "s", "tool-1", None);
    fs::write(f.repo.join("tracked.txt"), "one\ntwo changed\n").unwrap();
    f.hook(
        "PostToolUse",
        "s",
        "tool-1",
        Some("Set-Content tracked.txt"),
    );
    let all = f.events();
    let changes = select(&all, "ai_change");
    assert_eq!(changes.len(), 1);
    let e = changes[0];
    assert_eq!(e["before_sha"], sha(b"one\ntwo\n"));
    assert_eq!(e["after_sha"], sha(b"one\ntwo changed\n"));
    assert_eq!(e["line_ranges"], json!([[2, 2]]));
    assert_eq!(e["command_sha256"], sha(b"Set-Content tracked.txt"));
    assert_eq!(select(&all, "window_open").len(), 1);
    assert_eq!(select(&all, "window_close").len(), 1);
    f.drop_root();
}

#[test]
fn human_edit_is_external() {
    let f = Fixture::new("human");
    f.hook("PreToolUse", "s", "first", None);
    f.hook("PostToolUse", "s", "first", None);
    fs::write(f.repo.join("tracked.txt"), "human edit\n").unwrap();
    f.hook("PreToolUse", "s", "second", None);
    f.hook("PostToolUse", "s", "second", None);
    let all = f.events();
    assert!(select(&all, "external_change")
        .iter()
        .any(|e| e["after_sha"] == sha(b"human edit\n")));
    assert!(select(&all, "ai_change").is_empty());
    f.drop_root();
}

#[test]
fn file_filters_deletions_new_files_and_crlf() {
    let f = Fixture::new("files");
    f.hook("PreToolUse", "s", "t", None);
    fs::write(f.repo.join("new.txt"), "new\n").unwrap();
    fs::remove_file(f.repo.join("tracked.txt")).unwrap();
    fs::write(f.repo.join("crlf.txt"), "a\nb\n").unwrap();
    f.hook("PostToolUse", "s", "t", None);
    f.hook("PreToolUse", "s", "t2", None);
    fs::write(f.repo.join("crlf.txt"), "a\r\nb\r\n").unwrap();
    fs::write(f.repo.join("oversize.bin"), vec![b'x'; 1024 * 1024 + 1]).unwrap();
    fs::write(f.repo.join("binary.txt"), b"safe\0binary").unwrap();
    fs::write(f.repo.join(".env.local"), "do-not-store-this").unwrap();
    f.hook("PostToolUse", "s", "t2", None);
    let all = f.events();
    let changes = select(&all, "ai_change");
    assert!(changes
        .iter()
        .any(|e| e["path"] == "new.txt" && e["after_sha"] == sha(b"new\n")));
    assert!(changes
        .iter()
        .any(|e| e["path"] == "tracked.txt" && e["after_sha"].is_null()));
    assert!(
        changes
            .iter()
            .any(|e| e["path"] == "crlf.txt"
                && e["line_ranges"].as_array().is_some_and(Vec::is_empty))
    );
    assert!(!changes
        .iter()
        .any(|e| ["oversize.bin", "binary.txt", ".env.local"]
            .contains(&e["path"].as_str().unwrap_or(""))));
    let blobs = f.home.join("repos");
    let mut files = vec![blobs];
    let mut contents = Vec::new();
    while let Some(dir) = files.pop() {
        if let Ok(entries) = fs::read_dir(dir) {
            for e in entries.flatten() {
                if e.path().is_dir() {
                    files.push(e.path());
                } else if e.path().parent().is_some_and(|p| p.ends_with("blobs")) {
                    contents.push(fs::read(e.path()).unwrap());
                }
            }
        }
    }
    assert!(!contents
        .iter()
        .any(|b| b.windows(17).any(|w| w == b"do-not-store-this")));
    f.drop_root();
}

#[test]
fn overlapping_sessions_mark_both_windows() {
    let f = Fixture::new("overlap");
    f.hook("PreToolUse", "one", "a", None);
    f.hook("PreToolUse", "two", "b", None);
    fs::write(f.repo.join("tracked.txt"), "changed\n").unwrap();
    f.hook("PostToolUse", "one", "a", None);
    f.hook("PostToolUse", "two", "b", None);
    let all = f.events();
    let changes = select(&all, "ai_change");
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0]["overlap"], true);
    // The second window inherited its overlap state at open, even though the first closed first.
    let two = events_in(&f.home)
        .into_iter()
        .find(|e| e["event"] == "window_open" && e["session_id"] == "two")
        .unwrap();
    assert_eq!(two["overlap"], true);
    f.drop_root();
}

#[test]
fn torn_last_event_line_is_ignored_and_post_is_deduped() {
    let f = Fixture::new("torn");
    f.hook("PreToolUse", "s", "t", None);
    fs::write(f.repo.join("tracked.txt"), "x\n").unwrap();
    f.hook("PostToolUse", "s", "t", None);
    f.hook("PostToolUse", "s", "t", None);
    let all = f.events();
    assert_eq!(select(&all, "ai_change").len(), 1);
    f.hook("Stop", "s", "stop", None);
    assert_eq!(select(&f.events(), "turn_end").len(), 1);
    let path = find_event_file(&f.home);
    let mut file = fs::OpenOptions::new().append(true).open(path).unwrap();
    file.write_all(b"{torn").unwrap();
    assert_eq!(f.events().len(), all.len() + 1);
    f.drop_root();
}
fn find_event_file(home: &Path) -> PathBuf {
    let mut todo = vec![home.to_path_buf()];
    while let Some(dir) = todo.pop() {
        for e in fs::read_dir(dir).unwrap().flatten() {
            if e.path().is_dir() {
                todo.push(e.path());
            } else if e.path().extension().is_some_and(|x| x == "jsonl") {
                return e.path();
            }
        }
    }
    panic!("no event log")
}

#[test]
fn repo_identity_survives_move_and_unicode_space_path_works() {
    let f = Fixture::new("move");
    f.hook("Stop", "s", "t", None);
    let before = fs::read_dir(f.home.join("repos")).unwrap().count();
    let moved = f.root.join("moved 目录 with spaces");
    fs::rename(&f.repo, &moved).unwrap();
    f.hook_at(&moved, &f.home, "Stop", "s", "t2", None);
    let after = fs::read_dir(f.home.join("repos")).unwrap().count();
    assert_eq!(before, 1);
    assert_eq!(after, 1);
    f.drop_root();
}

#[test]
fn held_repo_lock_times_out_quickly_and_uses_inbox() {
    use fd_lock::RwLock;
    use std::fs::OpenOptions;
    use std::time::Instant;
    let f = Fixture::new("locked");
    let roots = git_text(&f.repo, &["rev-list", "--max-parents=0", "--all"]);
    let repo_id = sha(roots.as_bytes());
    let git_dir = git_text(&f.repo, &["rev-parse", "--absolute-git-dir"]);
    let worktree_id = sha(git_dir.as_bytes());
    let repo_store = f.home.join("repos").join(repo_id);
    fs::create_dir_all(&repo_store).unwrap();
    let lock_file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(repo_store.join("repo.lock"))
        .unwrap();
    let mut lock = RwLock::new(lock_file);
    let _guard = lock.try_write().expect("test process should acquire lock");
    let shadow = repo_store
        .join("shadow")
        .join(format!("{worktree_id}.json"));
    let start = Instant::now();
    let (code, out, err) = f.hook("PreToolUse", "locked-session", "locked-tool", None);
    assert_eq!(code, 0);
    assert!(out.is_empty() && err.is_empty());
    assert!(
        start.elapsed().as_secs_f64() < 1.2,
        "lock wait exceeded 1.2 seconds: {:?}",
        start.elapsed()
    );
    let inbox = fs::read_to_string(f.home.join("inbox.jsonl")).unwrap();
    assert!(inbox.contains("lock_timeout"));
    assert!(!shadow.exists());
    f.drop_root();
}

#[test]
fn a_slow_scan_hits_the_process_time_cap_and_records_degraded() {
    use std::time::Instant;
    let f = Fixture::new("slow");
    let bin = f.root.join("fake-bin");
    fs::create_dir_all(&bin).unwrap();
    let real_git = if cfg!(windows) { "where.exe" } else { "which" };
    let found = Command::new(real_git).arg("git").output().unwrap();
    assert!(found.status.success());
    let git_path = String::from_utf8_lossy(&found.stdout)
        .lines()
        .next()
        .unwrap()
        .trim()
        .to_owned();
    let source = format!(
        r##"use std::process::Command; use std::time::Duration; fn main() {{ let args: Vec<String> = std::env::args().skip(1).collect(); if args.iter().any(|x| x == "ls-files") {{ std::thread::sleep(Duration::from_secs(5)); return; }} let s = Command::new({:?}).args(args).status().unwrap(); std::process::exit(s.code().unwrap_or(1)); }}"##,
        git_path
    );
    let src = f.root.join("fake-git.rs");
    fs::write(&src, source).unwrap();
    let fake = bin.join(if cfg!(windows) { "git.exe" } else { "git" });
    let compile = Command::new("rustc")
        .arg(&src)
        .arg("-o")
        .arg(&fake)
        .status()
        .unwrap();
    assert!(compile.success());
    let old_path = std::env::var_os("PATH").unwrap_or_default();
    let sep = if cfg!(windows) { ";" } else { ":" };
    let path = format!("{}{}{}", bin.display(), sep, old_path.to_string_lossy());
    let payload = json!({"hook_event_name":"PreToolUse", "session_id":"slow", "turn_id":"t", "cwd":f.repo,
        "tool_use_id":"slow-tool", "tool_input":{"command":"slow"}});
    let start = Instant::now();
    let mut child = Command::new(env!("CARGO_BIN_EXE_prov-hook"))
        .arg("codex")
        .env("PATH", path)
        .env("PROV_HOME", &f.home)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.to_string().as_bytes())
        .unwrap();
    let status = child.wait().unwrap();
    let elapsed = start.elapsed();
    assert_eq!(status.code(), Some(0));
    assert!(
        elapsed.as_secs_f64() < 2.5,
        "time cap exceeded: {elapsed:?}"
    );
    let inbox = fs::read_to_string(f.home.join("inbox.jsonl")).unwrap();
    assert!(inbox.contains("time_cap"));
    f.drop_root();
}
