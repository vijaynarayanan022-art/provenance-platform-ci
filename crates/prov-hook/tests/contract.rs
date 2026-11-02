use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

fn isolated_command(program: impl AsRef<std::ffi::OsStr>) -> Command {
    let null = if cfg!(windows) { "NUL" } else { "/dev/null" };
    let mut command = Command::new(program);
    command
        .env("GIT_CONFIG_GLOBAL", null)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "core.hooksPath")
        .env("GIT_CONFIG_VALUE_0", null);
    command
}

static TEST_GATE: Mutex<()> = Mutex::new(());

fn test_guard() -> MutexGuard<'static, ()> {
    TEST_GATE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

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
        self.hook_at_with_path(cwd, home, event, session, tool_id, command, None)
    }
    #[allow(clippy::too_many_arguments)]
    fn hook_at_with_path(
        &self,
        cwd: &Path,
        home: &Path,
        event: &str,
        session: &str,
        tool_id: &str,
        command: Option<&str>,
        path: Option<&std::ffi::OsStr>,
    ) -> (i32, Vec<u8>, Vec<u8>) {
        let payload = json!({ "hook_event_name": event, "session_id": session, "turn_id": "turn-1", "cwd": cwd,
            "model": "test-model", "tool_name": "PowerShell", "tool_use_id": tool_id,
            "tool_input": { "command": command }, "transcript_path": "IGNORE_ME", "last_assistant_message": "IGNORE_ME" });
        let mut command = isolated_command(env!("CARGO_BIN_EXE_prov-hook"));
        command
            .arg("codex")
            .env("PROV_HOME", home)
            .env("PROV_NO_AUTODERIVE", "1");
        if let Some(path) = path {
            command.env("PATH", path);
        }
        let mut child = command
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
    let status = isolated_command("git")
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
    let out = isolated_command("git")
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
                    if let Ok(value) = serde_json::from_slice(line) {
                        out.push(value);
                    }
                }
            }
        }
    }
    out
}
fn sha(s: &[u8]) -> String {
    format!("{:x}", Sha256::digest(s))
}
fn hook_in_process(
    f: &Fixture,
    event: &str,
    session: &str,
    tool_id: &str,
) -> prov_capture::TimingSnapshot {
    std::env::set_var("PROV_HOME", &f.home);
    std::env::set_var("PROV_NO_AUTODERIVE", "1");
    let payload = json!({"hook_event_name":event,"session_id":session,"turn_id":"turn-1","cwd":f.repo,
        "model":"test-model","tool_name":"PowerShell","tool_use_id":tool_id,"tool_input":{"command":"test command"}});
    prov_capture::process_json_timed(payload.to_string().as_bytes())
}
fn select<'a>(events: &'a [Value], event: &str) -> Vec<&'a Value> {
    events.iter().filter(|e| e["event"] == event).collect()
}

#[test]
fn malformed_stdin_is_silent_and_successful() {
    let _serial = test_guard();
    let exe = env!("CARGO_BIN_EXE_prov-hook");
    for input in [
        Vec::new(),
        b"garbage".to_vec(),
        vec![b' '; 2 * 1024 * 1024],
        br#"{"cwd":[]}"#.to_vec(),
    ] {
        let mut child = isolated_command(exe)
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
    let _serial = test_guard();
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
    let _serial = test_guard();
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
    let _serial = test_guard();
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
    assert_eq!(e["command_length"], "Set-Content tracked.txt".len());
    assert_eq!(select(&all, "window_open").len(), 1);
    assert_eq!(select(&all, "window_close").len(), 1);
    f.drop_root();
}

#[test]
fn first_clean_scan_does_not_snapshot_unchanged_files() {
    let _serial = test_guard();
    let f = Fixture::new("clean-first");
    f.hook("PreToolUse", "s", "clean", None);
    let repo_dir = fs::read_dir(f.home.join("repos"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let shadow = fs::read_dir(repo_dir.join("shadow"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let shadow: Value = serde_json::from_slice(&fs::read(shadow).unwrap()).unwrap();
    assert!(shadow["files"].as_object().unwrap().is_empty());
    assert_eq!(fs::read_dir(repo_dir.join("blobs")).unwrap().count(), 0);
    assert!(select(&f.events(), "external_change").is_empty());
    f.drop_root();
}

#[test]
fn excluded_files_use_hash_only_shadow_and_emit_only_on_real_edits() {
    let _serial = test_guard();
    let f = Fixture::new("excluded-shadow");
    fs::write(f.repo.join(".env.local"), "env-original\n").unwrap();
    fs::write(f.repo.join("secrets.rs"), "secret-original\n").unwrap();
    git(&f.repo, &["add", ".env.local", "secrets.rs"]);
    git(&f.repo, &["commit", "-m", "add excluded fixtures"]);
    fs::write(f.repo.join(".env.local"), "env-first-seen\n").unwrap();
    fs::write(f.repo.join("secrets.rs"), "secret-first-seen\n").unwrap();
    for pair in 0..3 {
        if pair == 2 {
            fs::write(f.repo.join(".env.local"), "env-real-edit-longer\n").unwrap();
            fs::write(f.repo.join("secrets.rs"), "secret-real-edit-longer\n").unwrap();
        }
        let suffix = pair.to_string();
        f.hook("PreToolUse", "excluded", &format!("pre-{suffix}"), None);
        f.hook("PostToolUse", "excluded", &format!("post-{suffix}"), None);
    }
    let events = f.events();
    let external = select(&events, "external_change");
    assert_eq!(external.len(), 4);
    for event in &external {
        assert_eq!(event["excluded"], true);
        assert!(event["line_ranges"].is_null());
        assert!(event["after_sha"].as_str().is_some());
    }
    assert_eq!(
        external
            .iter()
            .filter(|event| event["before_sha"].is_null())
            .count(),
        2
    );
    let store = fs::read_dir(f.home.join("repos"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let shadow_path = fs::read_dir(store.join("shadow"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let shadow: Value = serde_json::from_slice(&fs::read(shadow_path).unwrap()).unwrap();
    for path in [".env.local", "secrets.rs"] {
        assert_eq!(shadow["files"][path]["excluded"], true);
        assert_eq!(shadow["files"][path]["deleted"], false);
        assert!(shadow["files"][path]["sha256"].as_str().is_some());
    }
    for entry in fs::read_dir(store.join("blobs")).unwrap() {
        let blob = fs::read(entry.unwrap().path()).unwrap();
        for secret in [
            b"env-first-seen".as_slice(),
            b"secret-first-seen".as_slice(),
            b"env-real-edit-longer".as_slice(),
            b"secret-real-edit-longer".as_slice(),
        ] {
            assert!(!blob.windows(secret.len()).any(|w| w == secret));
        }
    }
    f.drop_root();
}

#[test]
fn deletion_tombstone_emits_once_while_path_stays_dirty() {
    let _serial = test_guard();
    let f = Fixture::new("deletion-tombstone");
    f.hook("PreToolUse", "delete", "initial", None);
    fs::remove_file(f.repo.join("tracked.txt")).unwrap();
    for pair in 0..3 {
        f.hook("PreToolUse", "delete", &format!("pre-{pair}"), None);
        f.hook("PostToolUse", "delete", &format!("post-{pair}"), None);
    }
    let events = f.events();
    let deleted: Vec<_> = select(&events, "external_change")
        .into_iter()
        .filter(|event| event["path"] == "tracked.txt" && event["after_sha"].is_null())
        .collect();
    assert_eq!(deleted.len(), 1);
    assert_eq!(deleted[0]["deleted_at"], json!([0]));
    let store = fs::read_dir(f.home.join("repos"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let shadow_path = fs::read_dir(store.join("shadow"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let shadow: Value = serde_json::from_slice(&fs::read(shadow_path).unwrap()).unwrap();
    assert_eq!(shadow["files"]["tracked.txt"]["deleted"], true);
    f.drop_root();
}

#[test]
fn head_move_is_external_and_never_ai_credited() {
    let _serial = test_guard();
    let f = Fixture::new("head-move");
    let old_branch = git_text(&f.repo, &["branch", "--show-current"]);
    f.hook("PreToolUse", "s", "move-head", None);
    git(&f.repo, &["checkout", "-b", "other"]);
    fs::write(f.repo.join("tracked.txt"), "committed on other\n").unwrap();
    git(&f.repo, &["commit", "-am", "other change"]);
    fs::write(f.repo.join("tracked.txt"), "edited after head move\n").unwrap();
    f.hook("PostToolUse", "s", "move-head", None);
    let all = f.events();
    assert!(select(&all, "ai_change").is_empty());
    let external = select(&all, "external_change");
    assert!(external.iter().any(|e| e["reason"] == "head_moved"
        && e["path"] == "tracked.txt"
        && e["head_moved"] == true));
    assert!(external
        .iter()
        .any(|e| e["before_sha"] == sha(b"committed on other\n")
            && e["after_sha"] == sha(b"edited after head move\n")));
    let head_move = select(&all, "head_move")[0];
    assert_eq!(head_move["file_count"], 1);
    assert_eq!(head_move["paths"], json!(["tracked.txt"]));
    assert_eq!(head_move["commit_in_window"], true);
    assert_eq!(head_move["branch_before"], old_branch);
    assert_eq!(head_move["branch_after"], "other");
    assert_eq!(select(&all, "window_close")[0]["head_moved"], true);
    f.drop_root();
}

#[test]
fn thirty_file_commit_emits_one_bounded_head_move_with_few_git_spawns() {
    let _serial = test_guard();
    let f = Fixture::new("head-move-thirty");
    let pre = hook_in_process(&f, "PreToolUse", "many-commit", "many-tool");
    assert_eq!(pre.git_processes, 2);
    for i in 0..30 {
        fs::write(
            f.repo.join(format!("committed-{i:02}.txt")),
            format!("line {i}\n"),
        )
        .unwrap();
    }
    git(&f.repo, &["add", "--all"]);
    git(&f.repo, &["commit", "-m", "thirty files"]);
    let start = std::time::Instant::now();
    let post = hook_in_process(&f, "PostToolUse", "many-commit", "many-tool");
    assert!(start.elapsed() < std::time::Duration::from_millis(800));
    assert!(
        post.git_processes <= 4,
        "git process count: {}",
        post.git_processes
    );
    let head_move = select(&f.events(), "head_move")[0].clone();
    assert_eq!(head_move["file_count"], 30);
    assert_eq!(head_move["paths"].as_array().unwrap().len(), 30);
    assert_eq!(head_move["truncated"], false);
    assert_eq!(head_move["commit_in_window"], true);
    assert!(select(&f.events(), "ai_change").is_empty());
    f.drop_root();
}

#[test]
fn pull_between_hooks_records_changes_as_external() {
    let _serial = test_guard();
    let f = Fixture::new("head-pull");
    let bare = f.root.join("remote.git");
    git(
        &f.root,
        &[
            "clone",
            "--bare",
            f.repo.to_str().unwrap(),
            bare.to_str().unwrap(),
        ],
    );
    git(
        &f.repo,
        &["remote", "add", "origin", bare.to_str().unwrap()],
    );
    let peer = f.root.join("peer");
    git(
        &f.root,
        &["clone", bare.to_str().unwrap(), peer.to_str().unwrap()],
    );
    git(&peer, &["config", "user.email", "peer@example.invalid"]);
    git(&peer, &["config", "user.name", "Peer"]);
    fs::write(peer.join("tracked.txt"), "remote update\n").unwrap();
    git(&peer, &["commit", "-am", "remote update"]);
    git(&peer, &["push", "origin", "HEAD"]);
    f.hook("PreToolUse", "s", "pull", None);
    let branch = git_text(&f.repo, &["branch", "--show-current"]);
    git(&f.repo, &["pull", "--ff-only", "origin", branch.as_str()]);
    fs::write(f.repo.join("tracked.txt"), "after pull edit\n").unwrap();
    f.hook("PostToolUse", "s", "pull", None);
    let all = f.events();
    assert!(select(&all, "ai_change").is_empty());
    assert!(select(&all, "external_change")
        .iter()
        .any(|e| e["reason"] == "head_moved"
            && e["path"] == "tracked.txt"
            && e["after_sha"] == sha(b"after pull edit\n")));
    assert_eq!(select(&all, "head_move")[0]["commit_in_window"], true);
    f.drop_root();
}

#[test]
fn unrelated_branch_does_not_change_repo_identity() {
    let _serial = test_guard();
    let f = Fixture::new("orphan-identity");
    f.hook("Stop", "s", "first", None);
    let before = fs::read_dir(f.home.join("repos"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .file_name();
    git(&f.repo, &["branch", "unrelated"]);
    f.hook("Stop", "s", "second", None);
    let after = fs::read_dir(f.home.join("repos"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .file_name();
    assert_eq!(before, after);
    f.drop_root();
}

#[test]
fn second_hook_uses_repo_cache_without_spawning_rev_list() {
    let _serial = test_guard();
    let f = Fixture::new("repo-cache");
    f.hook("Stop", "s", "first", None);
    let cache = f.home.join("repo-cache");
    assert_eq!(fs::read_dir(&cache).unwrap().count(), 1);
    let bin = f.root.join("cache-wrapper");
    fs::create_dir_all(&bin).unwrap();
    let real_name = if cfg!(windows) { "where.exe" } else { "which" };
    let found = Command::new(real_name).arg("git").output().unwrap();
    assert!(found.status.success());
    let real_git = String::from_utf8_lossy(&found.stdout)
        .lines()
        .next()
        .unwrap()
        .trim()
        .to_owned();
    let count = f.root.join("rev-list-calls");
    let source = format!(
        r##"use std::process::Command; use std::io::Write; fn main() {{ let a:Vec<String>=std::env::args().skip(1).collect(); if a.iter().any(|x|x=="rev-list") {{ let mut f=std::fs::OpenOptions::new().create(true).append(true).open({:?}).unwrap(); writeln!(f,"rev-list").unwrap(); }} let s=Command::new({:?}).args(a).status().unwrap(); std::process::exit(s.code().unwrap_or(1)); }}"##,
        count, real_git
    );
    let src = f.root.join("cache-wrapper.rs");
    fs::write(&src, source).unwrap();
    let wrapper = bin.join(if cfg!(windows) { "git.exe" } else { "git" });
    assert!(Command::new("rustc")
        .arg(&src)
        .arg("-o")
        .arg(&wrapper)
        .status()
        .unwrap()
        .success());
    let old_path = std::env::var_os("PATH").unwrap_or_default();
    let sep = if cfg!(windows) { ";" } else { ":" };
    let path = std::ffi::OsString::from(format!(
        "{}{}{}",
        bin.display(),
        sep,
        old_path.to_string_lossy()
    ));
    let (code, out, err) =
        f.hook_at_with_path(&f.repo, &f.home, "Stop", "s", "cached", None, Some(&path));
    assert_eq!(code, 0);
    assert!(out.is_empty() && err.is_empty());
    assert!(!count.exists());
    f.drop_root();
}

#[test]
fn recreated_git_directory_recomputes_repo_identity() {
    let _serial = test_guard();
    let f = Fixture::new("repo-recreated");
    f.hook("Stop", "s", "old", None);
    let old_id = fs::read_dir(f.home.join("repos"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .file_name();
    fs::remove_dir_all(&f.repo).unwrap();
    fs::create_dir_all(&f.repo).unwrap();
    git(&f.repo, &["init"]);
    git(&f.repo, &["config", "user.email", "new@example.invalid"]);
    git(&f.repo, &["config", "user.name", "New Repo"]);
    fs::write(f.repo.join("new-root.txt"), "new root\n").unwrap();
    git(&f.repo, &["add", "."]);
    git(&f.repo, &["commit", "-m", "new repository root"]);
    f.hook("Stop", "s", "new", None);
    let ids: Vec<_> = fs::read_dir(f.home.join("repos"))
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert!(ids.iter().any(|id| id != &old_id));
    assert!(ids.contains(&old_id));
    f.drop_root();
}

#[test]
fn unmerged_paths_are_degraded_without_using_blob_ids() {
    let _serial = test_guard();
    let f = Fixture::new("unmerged");
    git(&f.repo, &["checkout", "-b", "conflict-side"]);
    fs::write(f.repo.join("tracked.txt"), "side change\n").unwrap();
    git(&f.repo, &["commit", "-am", "side"]);
    git(&f.repo, &["checkout", "-"]);
    fs::write(f.repo.join("tracked.txt"), "main change\n").unwrap();
    git(&f.repo, &["commit", "-am", "main"]);
    let merge = isolated_command("git")
        .args(["merge", "conflict-side"])
        .current_dir(&f.repo)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .unwrap();
    assert!(!merge.status.success());
    f.hook("PostToolUse", "conflict", "merge", None);
    let all = f.events();
    assert!(select(&all, "ai_change").is_empty());
    let conflict = select(&all, "external_change")
        .into_iter()
        .find(|event| event["path"] == "tracked.txt")
        .unwrap();
    assert_eq!(conflict["reason"], "unmerged");
    assert_eq!(conflict["degraded"], true);
    assert!(conflict["before_sha"].is_null() && conflict["after_sha"].is_null());
    f.drop_root();
}

#[test]
fn over_five_hundred_dirty_paths_emit_one_bulk_event_without_snapshots() {
    let _serial = test_guard();
    let f = Fixture::new("bulk");
    for i in 0..501 {
        fs::write(
            f.repo.join(format!("bulk-{i:03}.txt")),
            format!("secretish {i}\n"),
        )
        .unwrap();
    }
    f.hook("PreToolUse", "bulk", "bulk-tool", None);
    f.hook("PostToolUse", "bulk", "bulk-tool", None);
    let events = f.events();
    let bulk = select(&events, "bulk_change");
    assert_eq!(bulk.len(), 1);
    assert_eq!(bulk[0]["dirty_count"], 501);
    assert_eq!(bulk[0]["reason"], "bulk_change");
    assert_eq!(select(&events, "ai_change").len(), 0);
    assert_eq!(select(&events, "window_open")[0]["reason"], "bulk_change");
    let store = fs::read_dir(f.home.join("repos"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    assert!(
        fs::read_dir(store.join("blobs")).unwrap().count() <= 1,
        "bulk may snapshot only the single leftover file"
    );
    f.drop_root();
}

#[test]
fn shallow_repository_is_skipped_and_marked() {
    let _serial = test_guard();
    let f = Fixture::new("shallow");
    f.hook("Stop", "s", "initial", None);
    let head = git_text(&f.repo, &["rev-parse", "HEAD"]);
    fs::write(f.repo.join(".git").join("shallow"), format!("{head}\n")).unwrap();
    f.hook("Stop", "s", "shallow", None);
    let repo_id = fs::read_dir(f.home.join("repos"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .file_name();
    let markers =
        fs::read_to_string(f.home.join("repos").join(repo_id).join("markers.jsonl")).unwrap();
    assert!(markers.contains("shallow_repo_skipped"));
    assert_eq!(select(&f.events(), "turn_end").len(), 1);
    f.drop_root();
}

#[test]
fn fifty_thousand_old_events_do_not_make_hook_read_the_log() {
    let _serial = test_guard();
    use std::time::Instant;
    let f = Fixture::new("bounded-events");
    f.hook("Stop", "s", "init", None);
    let repo_dir = fs::read_dir(f.home.join("repos"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let old_log = repo_dir.join("events").join("old-session.jsonl");
    fs::write(&old_log, b"{}\n".repeat(50_000)).unwrap();
    let start = Instant::now();
    f.hook("Stop", "s", "after-old-events", None);
    let with_old_events = start.elapsed();
    let empty = Fixture::new("bounded-events-empty");
    let start = Instant::now();
    empty.hook("Stop", "s", "without-old-events", None);
    let without_old_events = start.elapsed();
    assert!(
        with_old_events < without_old_events * 4 + std::time::Duration::from_millis(400),
        "old-event cost {with_old_events:?}, baseline {without_old_events:?}"
    );
    f.drop_root();
    empty.drop_root();
}

#[cfg(windows)]
#[test]
fn unchanged_dirty_file_metadata_skips_locked_read() {
    use std::thread;
    use std::time::{Duration, Instant};
    let _serial = test_guard();
    let f = Fixture::new("unchanged-sharing");
    fs::write(f.repo.join("tracked.txt"), "dirty but stable\n").unwrap();
    f.hook("PreToolUse", "stable", "stable-tool", None);
    let file = f.repo.join("tracked.txt");
    let ready = f.root.join("stable-held-ready");
    let command = format!(
        "$f=[IO.File]::Open('{}',[IO.FileMode]::Open,[IO.FileAccess]::Read,[IO.FileShare]::None); Set-Content -LiteralPath '{}' ready; Start-Sleep -Seconds 20",
        file.display().to_string().replace('\'', "''"),
        ready.display().to_string().replace('\'', "''")
    );
    let mut holder = Command::new("powershell.exe")
        .args(["-NoProfile", "-Command", &command])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let start = Instant::now();
    while !ready.exists() && start.elapsed() < Duration::from_secs(5) {
        thread::sleep(Duration::from_millis(20));
    }
    assert!(ready.exists(), "holder did not acquire the file");
    f.hook("PostToolUse", "stable", "stable-tool", None);
    let _ = holder.kill();
    let _ = holder.wait();
    let events = f.events();
    assert_eq!(
        select(&events, "external_change")
            .iter()
            .filter(|e| e["path"] == "tracked.txt")
            .count(),
        1
    );
    assert!(select(&events, "ai_change").is_empty());
    assert!(!events.iter().any(|e| {
        e["event"] == "external_change" && e["path"] == "tracked.txt" && e["degraded"] == true
    }));
    f.drop_root();
}

#[cfg(windows)]
#[test]
fn identical_locked_blob_write_succeeds_and_preserves_blob() {
    use std::os::windows::fs::OpenOptionsExt;

    let _serial = test_guard();
    let fixture = Fixture::new("locked-identical-blob");
    fixture.hook("PreToolUse", "blob-tool", "blob-tool", None);
    let contents = b"identical content-addressed snapshot\n";
    fs::write(fixture.repo.join("tracked.txt"), contents).unwrap();
    let digest = sha(contents);
    let repo_id = fs::read_dir(fixture.home.join("repos"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .file_name()
        .to_string_lossy()
        .into_owned();
    let blob = fixture
        .home
        .join("repos")
        .join(repo_id)
        .join("blobs")
        .join(&digest);
    fs::write(&blob, contents).unwrap();

    let handle = fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(&blob)
        .unwrap();
    let (code, stdout, stderr) = fixture.hook("PostToolUse", "blob-tool", "blob-tool", None);
    assert_eq!(code, 0);
    assert!(stdout.is_empty() && stderr.is_empty());
    drop(handle);

    assert_eq!(fs::read(&blob).unwrap(), contents);
    let events = fixture.events();
    assert!(select(&events, "ai_change")
        .iter()
        .any(|event| event["path"] == "tracked.txt" && event["after_sha"] == digest));
    fixture.drop_root();
}

#[cfg(windows)]
#[test]
fn one_windows_sharing_violation_degrades_only_that_file() {
    let _serial = test_guard();
    use std::os::windows::fs::OpenOptionsExt;
    let f = Fixture::new("sharing-error");
    f.hook("PreToolUse", "s", "share", None);
    let file = f.repo.join("tracked.txt");
    fs::write(&file, "locked edit\n").unwrap();
    let handle = fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(&file)
        .unwrap();
    fs::write(f.repo.join("other.txt"), "another file\n").unwrap();
    f.hook("PostToolUse", "s", "share", None);
    drop(handle);
    let all = f.events();
    assert!(all.iter().any(|e| e["path"] == "tracked.txt"
        && e["reason"].as_str().is_some_and(|r| r.starts_with("open:"))));
    assert!(all
        .iter()
        .any(|e| e["path"] == "other.txt" && e["event"] == "ai_change"));
    f.drop_root();
}

#[cfg(unix)]
#[test]
fn one_unix_permission_error_degrades_only_that_file() {
    let _serial = test_guard();
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new("permission-error");
    f.hook("PreToolUse", "s", "perm", None);
    fs::write(f.repo.join("tracked.txt"), "changed under denied perms\n").unwrap();
    let file = f.repo.join("tracked.txt");
    let mut permissions = fs::metadata(&file).unwrap().permissions();
    permissions.set_mode(0o000);
    fs::set_permissions(&file, permissions).unwrap();
    if fs::read(&file).is_ok() {
        let mut p = fs::metadata(&file).unwrap().permissions();
        p.set_mode(0o644);
        let _ = fs::set_permissions(&file, p);
        f.drop_root();
        return;
    }
    fs::write(f.repo.join("other.txt"), "another file\n").unwrap();
    f.hook("PostToolUse", "s", "perm", None);
    let mut permissions = fs::metadata(&file).unwrap().permissions();
    permissions.set_mode(0o644);
    fs::set_permissions(&file, permissions).unwrap();
    let all = f.events();
    assert!(all.iter().any(|e| e["path"] == "tracked.txt"
        && e["reason"].as_str().is_some_and(|r| r.starts_with("open:"))));
    assert!(all
        .iter()
        .any(|e| e["path"] == "other.txt" && e["event"] == "ai_change"));
    f.drop_root();
}

#[test]
fn human_edit_is_external() {
    let _serial = test_guard();
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
    let _serial = test_guard();
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
    fs::write(f.repo.join("secrets.rs"), "let secret = \"hidden\";\n").unwrap();
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
        .any(|e| ["oversize.bin", "binary.txt"].contains(&e["path"].as_str().unwrap_or(""))));
    let excluded = changes
        .iter()
        .find(|e| e["path"] == ".env.local")
        .expect("excluded paths still have events");
    assert_eq!(excluded["excluded"], true);
    assert!(!excluded["after_sha"].is_null());
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
    assert!(!contents
        .iter()
        .any(|b| b.windows(6).any(|w| w == b"hidden")));
    assert!(changes
        .iter()
        .any(|e| e["path"] == "secrets.rs" && e["excluded"] == true && e["line_ranges"].is_null()));
    f.drop_root();
}

#[test]
fn overlapping_sessions_mark_both_windows() {
    let _serial = test_guard();
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
    let _serial = test_guard();
    let f = Fixture::new("torn");
    f.hook("PreToolUse", "s", "t", None);
    fs::write(f.repo.join("tracked.txt"), "x\n").unwrap();
    f.hook("PostToolUse", "s", "t", None);
    f.hook("PostToolUse", "s", "t", None);
    let all = f.events();
    assert_eq!(select(&all, "ai_change").len(), 1);
    f.hook("Stop", "s", "stop", None);
    assert_eq!(select(&f.events(), "turn_end").len(), 1);
    let path = f
        .home
        .join("repos")
        .join(
            fs::read_dir(f.home.join("repos"))
                .unwrap()
                .next()
                .unwrap()
                .unwrap()
                .file_name(),
        )
        .join("events")
        .join("s.jsonl");
    let mut file = fs::OpenOptions::new().append(true).open(path).unwrap();
    file.write_all(b"{torn").unwrap();
    assert_eq!(f.events().len(), all.len() + 1);
    f.hook("Stop", "s", "stop-after-torn", None);
    assert_eq!(select(&f.events(), "turn_end").len(), 2);
    f.drop_root();
}
#[test]
fn repo_identity_survives_move_and_unicode_space_path_works() {
    let _serial = test_guard();
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
fn held_repo_lock_times_out_quickly_and_uses_markers() {
    let _serial = test_guard();
    use fd_lock::RwLock;
    use std::fs::OpenOptions;
    use std::time::Instant;
    let f = Fixture::new("locked");
    let roots = git_text(
        &f.repo,
        &["rev-list", "--first-parent", "--max-parents=0", "HEAD"],
    );
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
        start.elapsed().as_millis() >= 450,
        "lock timeout returned before the 500 ms budget: {:?}",
        start.elapsed()
    );
    assert!(
        start.elapsed().as_secs_f64() < 1.2,
        "lock wait exceeded 1.2 seconds: {:?}",
        start.elapsed()
    );
    let markers = fs::read_to_string(repo_store.join("markers.jsonl")).unwrap();
    assert!(markers.contains("lock_timeout"));
    assert!(markers.contains("\"hook_event_name\":\"PreToolUse\""));
    assert!(markers.contains("\"tool_use_id\":\"locked-tool\""));
    assert!(markers.contains("\"session_id\":\"locked-session\""));
    assert!(!shadow.exists());
    assert!(select(&events_in(&f.home), "window_open").is_empty());
    f.drop_root();
}

#[test]
fn a_slow_scan_hits_the_process_time_cap_and_records_degraded() {
    let _serial = test_guard();
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
        r##"use std::process::Command; use std::time::Duration; fn main() {{ let args: Vec<String> = std::env::args().skip(1).collect(); if args.iter().any(|x| x == "status") {{ std::thread::sleep(Duration::from_secs(5)); return; }} let s = Command::new({:?}).args(args).status().unwrap(); std::process::exit(s.code().unwrap_or(1)); }}"##,
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
    let mut child = isolated_command(env!("CARGO_BIN_EXE_prov-hook"))
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
    let repo_id = sha(git_text(
        &f.repo,
        &["rev-list", "--first-parent", "--max-parents=0", "HEAD"],
    )
    .as_bytes());
    let markers =
        fs::read_to_string(f.home.join("repos").join(repo_id).join("markers.jsonl")).unwrap();
    assert!(markers.contains("time_cap"));
    f.drop_root();
}

#[test]
fn tracked_dirty_file_with_deleted_snapshot_degrades_and_never_ai_credited() {
    let _serial = test_guard();
    let f = Fixture::new("deleted-snapshot");
    fs::write(f.repo.join("tracked.txt"), "one\ntwo dirty\n").unwrap();
    f.hook("PreToolUse", "s", "tool-1", None);

    let store = fs::read_dir(f.home.join("repos"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let blobs_dir = store.join("blobs");
    for entry in fs::read_dir(&blobs_dir).unwrap().flatten() {
        fs::remove_file(entry.path()).unwrap();
    }

    fs::write(f.repo.join("tracked.txt"), "one\ntwo dirty edited\n").unwrap();
    f.hook("PostToolUse", "s", "tool-1", Some("edit tracked.txt"));

    let all = f.events();
    assert!(
        select(&all, "ai_change").is_empty(),
        "must never credit missing before as ai_change"
    );

    let external = select(&all, "external_change");
    let target = external
        .iter()
        .find(|e| e["path"] == "tracked.txt" && e["hook_event_name"] == "PostToolUse")
        .expect("must emit external_change from PostToolUse");
    assert_eq!(target["reason"], "before_unavailable");
    assert_eq!(target["degraded"], true);
    assert!(target["line_ranges"].is_null());

    f.drop_root();
}

#[test]
fn corrupt_snapshot_hash_mismatch_degrades_and_never_ai_credited() {
    let _serial = test_guard();
    let f = Fixture::new("corrupt-snapshot");
    fs::write(f.repo.join("tracked.txt"), "one\ntwo dirty\n").unwrap();
    f.hook("PreToolUse", "s", "tool-1", None);

    let store = fs::read_dir(f.home.join("repos"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let blobs_dir = store.join("blobs");
    for entry in fs::read_dir(&blobs_dir).unwrap().flatten() {
        fs::write(entry.path(), b"corrupted snapshot bytes").unwrap();
    }

    fs::write(f.repo.join("tracked.txt"), "one\ntwo dirty edited\n").unwrap();
    f.hook("PostToolUse", "s", "tool-1", Some("edit tracked.txt"));

    let all = f.events();
    assert!(
        select(&all, "ai_change").is_empty(),
        "must never credit corrupt before as ai_change"
    );

    let external = select(&all, "external_change");
    let target = external
        .iter()
        .find(|e| e["path"] == "tracked.txt" && e["hook_event_name"] == "PostToolUse")
        .expect("must emit external_change from PostToolUse");
    assert_eq!(target["reason"], "before_unavailable");
    assert_eq!(target["degraded"], true);
    assert!(target["line_ranges"].is_null());

    f.drop_root();
}

#[test]
fn missing_head_blob_degrades_and_never_ai_credited() {
    let _serial = test_guard();
    let f = Fixture::new("missing-head-blob");
    let blob_sha = git_text(&f.repo, &["rev-parse", "HEAD:tracked.txt"]);
    let obj_path = f
        .repo
        .join(".git")
        .join("objects")
        .join(&blob_sha[..2])
        .join(&blob_sha[2..]);
    fs::remove_file(obj_path).unwrap();

    fs::write(f.repo.join("tracked.txt"), "one\ntwo changed\n").unwrap();
    f.hook("PostToolUse", "s", "tool-1", Some("edit tracked.txt"));

    let all = f.events();
    assert!(
        select(&all, "ai_change").is_empty(),
        "must not credit missing head blob as ai_change"
    );

    let external = select(&all, "external_change");
    let target = external
        .iter()
        .find(|e| e["path"] == "tracked.txt")
        .expect("must emit external_change");
    assert_eq!(target["reason"], "before_unavailable");
    assert_eq!(target["degraded"], true);
    assert!(target["line_ranges"].is_null());

    f.drop_root();
}

#[test]
fn batch_with_one_missing_blob_degrades_only_missing_one() {
    let _serial = test_guard();
    let f = Fixture::new("batch-missing-blob");
    fs::write(f.repo.join("second.txt"), "alpha\nbeta\n").unwrap();
    git(&f.repo, &["add", "second.txt"]);
    git(&f.repo, &["commit", "-m", "add second"]);

    let missing_sha = git_text(&f.repo, &["rev-parse", "HEAD:tracked.txt"]);
    let obj_path = f
        .repo
        .join(".git")
        .join("objects")
        .join(&missing_sha[..2])
        .join(&missing_sha[2..]);
    fs::remove_file(obj_path).unwrap();

    fs::write(f.repo.join("tracked.txt"), "one\ntwo modified\n").unwrap();
    fs::write(f.repo.join("second.txt"), "alpha\nbeta modified\n").unwrap();
    f.hook("PostToolUse", "s", "tool-1", Some("edit both"));

    let all = f.events();
    let ai = select(&all, "ai_change");
    assert_eq!(ai.len(), 1, "only valid blob gets ai_change");
    assert_eq!(ai[0]["path"], "second.txt");
    assert_eq!(ai[0]["line_ranges"], json!([[2, 2]]));

    let external = select(&all, "external_change");
    let target = external
        .iter()
        .find(|e| e["path"] == "tracked.txt")
        .expect("tracked.txt must degrade");
    assert_eq!(target["reason"], "before_unavailable");
    assert_eq!(target["degraded"], true);
    assert!(target["line_ranges"].is_null());

    f.drop_root();
}

#[test]
fn bulk_cap_processes_candidates_incrementally_and_resumes_normal_scans() {
    let _serial = test_guard();
    let f = Fixture::new("bulk-incremental");
    for i in 0..1050 {
        fs::write(
            f.repo.join(format!("stable-{i:04}.txt")),
            format!("content {i}\n"),
        )
        .unwrap();
    }

    f.hook("PreToolUse", "s", "tool-1", None);
    let events1 = f.events();
    let bulk1 = select(&events1, "bulk_change");
    assert_eq!(bulk1.len(), 1);
    assert_eq!(bulk1[0]["remaining"], 550);

    f.hook("PostToolUse", "s", "tool-1", None);
    let events2 = f.events();
    assert_eq!(select(&events2, "bulk_change").len(), 1);
    let close1 = select(&events2, "window_close");
    assert_eq!(close1.len(), 1);
    assert_eq!(close1[0]["reason"], "bulk_change");
    assert_eq!(close1[0]["degraded"], true);

    // Only 50 candidates remain: scans are normal again, no new bulk event.
    f.hook("PreToolUse", "s", "tool-2", None);
    f.hook("PostToolUse", "s", "tool-2", None);
    let events3 = f.events();
    assert_eq!(select(&events3, "bulk_change").len(), 1);

    f.hook("PreToolUse", "s", "tool-3", None);
    fs::write(f.repo.join("tracked.txt"), "one\ntwo AI edit\n").unwrap();
    f.hook("PostToolUse", "s", "tool-3", Some("edit tracked.txt"));

    let events4 = f.events();
    let ai = select(&events4, "ai_change");
    assert_eq!(ai.len(), 1);
    assert_eq!(ai[0]["path"], "tracked.txt");
    assert_eq!(ai[0]["line_ranges"], json!([[2, 2]]));

    f.drop_root();
}
#[test]
fn rename_without_content_change_does_not_emit_ai_change() {
    let _serial = test_guard();
    let f = Fixture::new("rename-no-ai");
    f.hook("PreToolUse", "s", "tool-rename", None);
    git(&f.repo, &["mv", "tracked.txt", "renamed.txt"]);
    f.hook("PostToolUse", "s", "tool-rename", None);

    let events = f.events();
    assert!(select(&events, "ai_change").is_empty());
    f.drop_root();
}

#[test]
fn large_head_blob_over_one_mib_is_skipped_and_degrades() {
    let _serial = test_guard();
    let f = Fixture::new("large-blob");
    let large_data = vec![b'a'; 3 * 1024 * 1024];
    fs::write(f.repo.join("large.txt"), &large_data).unwrap();
    git(&f.repo, &["add", "large.txt"]);
    git(&f.repo, &["commit", "-m", "add 3mb file"]);

    fs::write(f.repo.join("large.txt"), "small change\n").unwrap();
    let start = std::time::Instant::now();
    f.hook("PostToolUse", "s", "tool-large", Some("edit large"));
    let elapsed = start.elapsed();
    assert!(
        elapsed < std::time::Duration::from_millis(1500),
        "hook should finish quickly: {:?}",
        elapsed
    );

    let all = f.events();
    assert!(
        select(&all, "ai_change").is_empty(),
        "large blob must never be ai credited"
    );
    let external = select(&all, "external_change");
    let target = external
        .iter()
        .find(|e| e["path"] == "large.txt")
        .expect("must emit external_change");
    assert_eq!(target["reason"], "before_unavailable");
    assert_eq!(target["degraded"], true);
    assert!(target["line_ranges"].is_null());

    f.drop_root();
}
