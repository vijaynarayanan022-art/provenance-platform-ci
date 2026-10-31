use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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

struct Fixture {
    root: PathBuf,
    repo: PathBuf,
    home: PathBuf,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "prov-derive-{name}-{}-{}",
            std::process::id(),
            now()
        ));
        let repo = root.join("repository with spaces");
        let home = root.join("store outside repository");
        fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init"]);
        fs::write(repo.join("tracked.txt"), b"one\ntwo\n").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-m", "initial"]);
        Self { root, repo, home }
    }

    fn hook(&self, event: &str, tool: &str) {
        let _ = self.hook_with_autoderive(event, tool, false);
    }

    fn hook_with_autoderive(&self, event: &str, tool: &str, auto_derive: bool) -> Duration {
        let payload = json!({
            "hook_event_name": event,
            "session_id": "derive-test-session",
            "turn_id": "derive-test-turn",
            "cwd": self.repo,
            "tool_use_id": tool,
            "tool_name": "PowerShell",
            "tool_input": {"command": "test command"},
        });
        let mut command = isolated_command(env!("CARGO_BIN_EXE_prov-hook"));
        command.arg("codex").env("PROV_HOME", &self.home);
        if auto_derive {
            command.env_remove("PROV_NO_AUTODERIVE");
        } else {
            command.env("PROV_NO_AUTODERIVE", "1");
        }
        let started = Instant::now();
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
        let status = child.wait().unwrap();
        let elapsed = started.elapsed();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        child
            .stdout
            .take()
            .unwrap()
            .read_to_end(&mut stdout)
            .unwrap();
        child
            .stderr
            .take()
            .unwrap()
            .read_to_end(&mut stderr)
            .unwrap();
        assert_eq!(status.code(), Some(0));
        assert!(stdout.is_empty());
        assert!(stderr.is_empty(), "{}", String::from_utf8_lossy(&stderr));
        elapsed
    }

    fn cli(&self, args: &[&str]) -> Output {
        isolated_command(env!("CARGO_BIN_EXE_prov-hook"))
            .args(args)
            .env("PROV_HOME", &self.home)
            .output()
            .unwrap()
    }

    fn commit(&self, message: &str) -> String {
        git(&self.repo, &["add", "-A"]);
        git(&self.repo, &["commit", "-m", message]);
        git_text(&self.repo, &["rev-parse", "HEAD"])
    }

    fn repo_id(&self) -> String {
        fs::read_dir(self.home.join("repos"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .file_name()
            .to_string_lossy()
            .into_owned()
    }

    fn derived_path(&self, commit: &str) -> PathBuf {
        self.home
            .join("repos")
            .join(self.repo_id())
            .join("commits")
            .join(format!("{commit}.json"))
    }

    fn lines_path(&self, commit: &str) -> PathBuf {
        self.derived_path(commit)
            .with_file_name(format!("{commit}.lines.json"))
    }

    fn derived_value(&self, commit: &str) -> Value {
        serde_json::from_slice(&fs::read(self.derived_path(commit)).unwrap()).unwrap()
    }

    fn sidecar_value(&self, commit: &str) -> Value {
        serde_json::from_slice(&fs::read(self.lines_path(commit)).unwrap()).unwrap()
    }

    fn pending(&self) -> Output {
        self.cli(&["derive", "--pending", "--repo", self.repo.to_str().unwrap()])
    }

    fn doctor(&self) -> Output {
        let codex_home = self.root.join("Codex Home");
        fs::create_dir_all(&codex_home).unwrap();
        let codex_home = codex_home.to_str().unwrap();
        let install = self.cli(&[
            "install",
            "--codex-home",
            codex_home,
            "--enable-hooks-feature",
        ]);
        assert!(install.status.success());
        self.cli(&[
            "doctor",
            "--codex-home",
            codex_home,
            "--repo",
            self.repo.to_str().unwrap(),
        ])
    }

    fn queue_head_moves(&self, commits: &[String]) {
        let event_dir = self.home.join("repos").join(self.repo_id()).join("events");
        fs::create_dir_all(&event_dir).unwrap();
        let path = event_dir.join("derive-test-session.jsonl");
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        for commit in commits {
            writeln!(
                file,
                "{}",
                json!({"event":"head_move", "timestamp":now(), "new_head":commit})
            )
            .unwrap();
        }
    }

    fn clear_events(&self) {
        let event_dir = self.home.join("repos").join(self.repo_id()).join("events");
        if let Ok(entries) = fs::read_dir(event_dir) {
            for entry in entries.flatten() {
                let _ = fs::remove_file(entry.path());
            }
        }
    }

    fn commits(&self, count: usize) -> Vec<String> {
        (0..count)
            .map(|index| {
                fs::write(
                    self.repo.join("tracked.txt"),
                    format!("base\ncommit line {index}\n"),
                )
                .unwrap();
                self.commit(&format!("pending {index}"))
            })
            .collect()
    }

    fn empty_commits(&self, count: usize) -> Vec<String> {
        for index in 0..count {
            let message = format!("empty pending {index}");
            git(&self.repo, &["commit", "--allow-empty", "-m", &message]);
        }
        let range = format!("HEAD~{count}..HEAD");
        git_text(&self.repo, &["rev-list", "--reverse", &range])
            .lines()
            .map(str::to_owned)
            .collect()
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

fn set_age(path: &Path, age: Duration) {
    let file = fs::OpenOptions::new().write(true).open(path).unwrap();
    file.set_times(fs::FileTimes::new().set_modified(SystemTime::now() - age))
        .unwrap();
}

fn patch_id(repo: &Path, parent: &str, commit: &str) -> String {
    let diff = isolated_command("git")
        .args(["diff-tree", "-p", "--no-renames", parent, commit])
        .current_dir(repo)
        .output()
        .unwrap();
    assert!(diff.status.success());
    let mut child = isolated_command("git")
        .args(["patch-id", "--stable"])
        .current_dir(repo)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(&diff.stdout).unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    String::from_utf8(output.stdout)
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .to_owned()
}

fn git(repo: &Path, args: &[&str]) {
    let output = isolated_command("git")
        .args(args)
        .current_dir(repo)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_AUTHOR_NAME", "Derive Test")
        .env("GIT_AUTHOR_EMAIL", "derive@example.invalid")
        .env("GIT_COMMITTER_NAME", "Derive Test")
        .env("GIT_COMMITTER_EMAIL", "derive@example.invalid")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(output.success(), "git {args:?} failed");
}

fn git_text(repo: &Path, args: &[&str]) -> String {
    let output = isolated_command("git")
        .args(args)
        .current_dir(repo)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?} failed");
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn git_blob(repo: &Path, bytes: &[u8]) -> String {
    let mut child = isolated_command("git")
        .args(["hash-object", "-w", "--stdin"])
        .current_dir(repo)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(bytes).unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn derive(fixture: &Fixture, commit: &str) -> Output {
    fixture.cli(&[
        "derive",
        "--repo",
        fixture.repo.to_str().unwrap(),
        "--commit",
        commit,
    ])
}

fn ai_commit_with_eols(name: &str, autocrlf: &str, content: &[u8]) -> Value {
    let fixture = Fixture::new(name);
    git(&fixture.repo, &["config", "core.autocrlf", autocrlf]);
    fixture.hook("PreToolUse", "ai-eol-tool");
    fs::write(fixture.repo.join("tracked.txt"), content).unwrap();
    fixture.hook("PostToolUse", "ai-eol-tool");
    let commit = fixture.commit("AI edit with line endings");
    let result = derive(&fixture, &commit);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let value = fixture.derived_value(&commit);
    fixture.drop_root();
    value
}

fn file<'a>(value: &'a Value, path: &str) -> &'a Value {
    value["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["path"] == path)
        .unwrap_or_else(|| panic!("missing file {path}"))
}

fn assert_counts_consistent(value: &Value) {
    for counts in value["files"].as_array().unwrap() {
        assert_eq!(
            counts["e"].as_u64().unwrap(),
            counts["ai_strong"].as_u64().unwrap()
                + counts["ai_probable"].as_u64().unwrap()
                + counts["mixed"].as_u64().unwrap()
                + counts["unknown"].as_u64().unwrap()
        );
    }
    let totals = &value["totals"];
    assert_eq!(
        totals["e"].as_u64().unwrap(),
        totals["ai_strong"].as_u64().unwrap()
            + totals["ai_probable"].as_u64().unwrap()
            + totals["mixed"].as_u64().unwrap()
            + totals["unknown"].as_u64().unwrap()
    );
}

fn assert_sidecar_counts_match(report: &Value, sidecar: &Value) {
    for key in ["e", "ai_strong", "ai_probable", "mixed", "unknown"] {
        assert_eq!(sidecar["totals"][key], report["totals"][key], "{key}");
    }
}

fn line_label<'a>(sidecar: &'a Value, path: &str, hash: &str) -> &'a str {
    sidecar["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|file| file["path"] == path)
        .unwrap()["lines"]
        .as_array()
        .unwrap()
        .iter()
        .find(|line| line["hash"] == hash)
        .unwrap()["label"]
        .as_str()
        .unwrap()
}

fn ai_commit(fixture: &Fixture, contents: &str, message: &str) -> String {
    fixture.hook("PreToolUse", message);
    fs::write(fixture.repo.join("tracked.txt"), contents).unwrap();
    fixture.hook("PostToolUse", message);
    let commit = fixture.commit(message);
    let result = derive(fixture, &commit);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    commit
}

fn line_hash(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))[..16].to_owned()
}

#[test]
fn ai_and_human_commits_are_classified_with_evidence() {
    let ai = Fixture::new("ai-exact");
    ai.hook("PreToolUse", "ai-tool");
    fs::write(ai.repo.join("tracked.txt"), b"one\nai line\n").unwrap();
    ai.hook("PostToolUse", "ai-tool");
    let commit = ai.commit("commit AI edit");
    let result = derive(&ai, &commit);
    assert!(result.status.success());
    let value = ai.derived_value(&commit);
    let tracked = file(&value, "tracked.txt");
    assert_eq!(tracked["evidence"], "exact");
    assert_eq!(tracked["ai_strong"], 1);
    assert_counts_consistent(&value);
    ai.drop_root();

    let human = Fixture::new("human-only");
    fs::write(human.repo.join("tracked.txt"), b"one\nhuman line\n").unwrap();
    let commit = human.commit("human edit without hook evidence");
    assert!(derive(&human, &commit).status.success());
    let value = human.derived_value(&commit);
    let tracked = file(&value, "tracked.txt");
    assert_eq!(tracked["unknown"], 1);
    assert_eq!(tracked["evidence"], "none");
    assert!(matches!(value["coverage"].as_str(), Some("none" | "low")));
    human.drop_root();
}

#[test]
fn autocrlf_true_matches_crlf_snapshot_to_lf_commit() {
    let value = ai_commit_with_eols("autocrlf-true", "true", b"one\r\nai line\r\n");
    let tracked = file(&value, "tracked.txt");
    assert_eq!(tracked["evidence"], "exact");
    assert_eq!(tracked["ai_strong"], 1);
    assert_eq!(tracked["e"], 1);
}

#[test]
fn autocrlf_false_matches_crlf_snapshot_and_commit() {
    let value = ai_commit_with_eols("autocrlf-false", "false", b"one\r\nai line\r\n");
    let tracked = file(&value, "tracked.txt");
    assert_eq!(tracked["evidence"], "exact");
    assert_eq!(tracked["ai_strong"], 1);
    assert_eq!(tracked["e"], 1);
}

#[test]
fn mixed_line_endings_derive_consistent_counts() {
    let value = ai_commit_with_eols("mixed-eols", "false", b"one\r\nai line\nthird\r\n");
    let tracked = file(&value, "tracked.txt");
    assert_eq!(tracked["evidence"], "exact");
    assert_counts_consistent(&value);
}

#[test]
fn human_edit_after_ai_snapshot_is_mixed() {
    let fixture = Fixture::new("mixed");
    fixture.hook("PreToolUse", "ai-tool");
    fs::write(fixture.repo.join("tracked.txt"), b"one\nai line\n").unwrap();
    fixture.hook("PostToolUse", "ai-tool");
    fs::write(fixture.repo.join("tracked.txt"), b"one\nhuman edit\n").unwrap();
    fixture.hook("PreToolUse", "human-tool");
    fixture.hook("PostToolUse", "human-tool");
    let commit = fixture.commit("human changes AI line");
    assert!(derive(&fixture, &commit).status.success());
    let value = fixture.derived_value(&commit);
    let tracked = file(&value, "tracked.txt");
    assert_eq!(tracked["mixed"], 1);
    assert_eq!(tracked["evidence"], "exact");
    fixture.drop_root();
}

#[test]
fn edits_after_last_hook_use_time_cut_and_degrade_ai() {
    let fixture = Fixture::new("time-cut");
    fixture.hook("PreToolUse", "ai-tool");
    fs::write(fixture.repo.join("tracked.txt"), b"one\nai line\n").unwrap();
    fixture.hook("PostToolUse", "ai-tool");
    fs::write(
        fixture.repo.join("tracked.txt"),
        b"one\nai line\nlate line\n",
    )
    .unwrap();
    let commit = fixture.commit("edit after last hook");

    assert!(derive(&fixture, &commit).status.success());
    let value = fixture.derived_value(&commit);
    let tracked = file(&value, "tracked.txt");
    assert_eq!(tracked["evidence"], "time_cut");
    assert!(tracked["ai_probable"].as_u64().unwrap() >= 1);
    assert_eq!(tracked["ai_strong"], 0);
    fixture.drop_root();
}

#[test]
fn overlap_or_degraded_ai_events_are_probable() {
    for (name, field) in [("overlap", "overlap"), ("degraded", "degraded")] {
        let fixture = Fixture::new(name);
        fixture.hook("PreToolUse", "ai-tool");
        fs::write(fixture.repo.join("tracked.txt"), b"one\nai line\n").unwrap();
        fixture.hook("PostToolUse", "ai-tool");
        let event_path = fs::read_dir(
            fixture
                .home
                .join("repos")
                .join(fixture.repo_id())
                .join("events"),
        )
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
        let mut output = Vec::new();
        for line in fs::read(&event_path).unwrap().split(|byte| *byte == b'\n') {
            if line.is_empty() {
                continue;
            }
            let mut value: Value = serde_json::from_slice(line).unwrap();
            if value["event"] == "ai_change" && value["path"] == "tracked.txt" {
                value[field] = true.into();
            }
            serde_json::to_writer(&mut output, &value).unwrap();
            output.push(b'\n');
        }
        fs::write(event_path, output).unwrap();
        let commit = fixture.commit("degraded AI event");
        assert!(derive(&fixture, &commit).status.success());
        let result = fixture.derived_value(&commit);
        let tracked = file(&result, "tracked.txt");
        assert_eq!(tracked["ai_probable"], 1, "case {name}");
        assert_eq!(tracked["ai_strong"], 0, "case {name}");
        fixture.drop_root();
    }
}

#[test]
fn range_derivation_does_not_carry_stale_parent_events() {
    let fixture = Fixture::new("stale-events");
    let base = git_text(&fixture.repo, &["rev-parse", "HEAD"]);

    fixture.hook("PreToolUse", "first-ai");
    fs::write(fixture.repo.join("tracked.txt"), b"one\nfirst AI\n").unwrap();
    fixture.hook("PostToolUse", "first-ai");
    let first = fixture.commit("first AI edit");

    fixture.hook("PreToolUse", "second-ai");
    fs::write(
        fixture.repo.join("tracked.txt"),
        b"one\nfirst AI\nsecond AI\n",
    )
    .unwrap();
    fixture.hook("PostToolUse", "second-ai");
    let second = fixture.commit("second AI edit");

    let range = format!("{base}..{second}");
    let result = fixture.cli(&[
        "derive",
        "--repo",
        fixture.repo.to_str().unwrap(),
        "--range",
        &range,
    ]);
    assert!(result.status.success());
    let value = fixture.derived_value(&second);
    let tracked = file(&value, "tracked.txt");
    assert_eq!(tracked["e"], 1);
    assert_eq!(tracked["ai_strong"], 1);
    assert_eq!(tracked["evidence"], "exact");
    assert!(fixture.derived_path(&first).exists());
    fixture.drop_root();
}

#[test]
fn commit_inside_ai_window_is_reconstructed_as_probable() {
    let fixture = Fixture::new("reconstructed");
    fixture.hook("PreToolUse", "commit-tool");
    fs::write(fixture.repo.join("tracked.txt"), b"one\nai line\n").unwrap();
    let commit = fixture.commit("agent commit");
    fixture.hook("PostToolUse", "commit-tool");
    assert!(derive(&fixture, &commit).status.success());
    let value = fixture.derived_value(&commit);
    let tracked = file(&value, "tracked.txt");
    assert_eq!(tracked["ai_probable"], 1);
    assert_eq!(tracked["evidence"], "reconstructed");
    fixture.drop_root();
}

#[test]
fn missing_events_and_missing_snapshot_blobs_are_unknown() {
    let no_events = Fixture::new("no-events");
    fs::write(no_events.repo.join("tracked.txt"), b"one\nhuman line\n").unwrap();
    let commit = no_events.commit("no event evidence");
    assert!(derive(&no_events, &commit).status.success());
    let value = no_events.derived_value(&commit);
    assert_eq!(file(&value, "tracked.txt")["unknown"], 1);
    assert_eq!(value["coverage"], "none");
    no_events.drop_root();

    let missing = Fixture::new("missing-blob");
    missing.hook("PreToolUse", "ai-tool");
    fs::write(missing.repo.join("tracked.txt"), b"one\nai line\n").unwrap();
    missing.hook("PostToolUse", "ai-tool");
    let events_dir = missing
        .home
        .join("repos")
        .join(missing.repo_id())
        .join("events");
    let event_path = fs::read_dir(&events_dir)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let lines = fs::read(&event_path).unwrap();
    let ai_event = lines
        .split(|byte| *byte == b'\n')
        .filter_map(|line| serde_json::from_slice::<Value>(line).ok())
        .find(|event| event["event"] == "ai_change" && event["path"] == "tracked.txt")
        .unwrap();
    let snapshot = ai_event["after_sha"].as_str().unwrap();
    fs::remove_file(
        missing
            .home
            .join("repos")
            .join(missing.repo_id())
            .join("blobs")
            .join(snapshot),
    )
    .unwrap();
    use std::fs::OpenOptions;
    let mut log = OpenOptions::new().append(true).open(event_path).unwrap();
    log.write_all(b"{corrupt event line}\n").unwrap();
    let commit = missing.commit("missing event blob");
    assert!(derive(&missing, &commit).status.success());
    let value = missing.derived_value(&commit);
    let tracked = file(&value, "tracked.txt");
    assert_eq!(tracked["unknown"], 1);
    assert_eq!(tracked["evidence"], "missing_blob");
    missing.drop_root();
}

#[test]
fn unreadable_parent_and_invalid_event_hashes_are_counted_safely() {
    let parent_missing = Fixture::new("missing-parent-blob");
    let parent_blob = git_text(&parent_missing.repo, &["rev-parse", "HEAD:tracked.txt"]);
    fs::write(parent_missing.repo.join("tracked.txt"), b"one\nchanged\n").unwrap();
    let commit = parent_missing.commit("parent blob unavailable");
    let object = parent_missing
        .repo
        .join(".git/objects")
        .join(&parent_blob[..2])
        .join(&parent_blob[2..]);
    fs::remove_file(object).unwrap();
    assert!(derive(&parent_missing, &commit).status.success());
    let value = parent_missing.derived_value(&commit);
    assert_eq!(file(&value, "tracked.txt")["evidence"], "missing_blob");
    assert_eq!(file(&value, "tracked.txt")["e"], 0);
    assert_eq!(value["totals"]["unreadable_files"], 1);
    parent_missing.drop_root();

    let malformed = Fixture::new("invalid-event-hash");
    malformed.hook("PreToolUse", "ai-tool");
    fs::write(malformed.repo.join("tracked.txt"), b"one\nai line\n").unwrap();
    malformed.hook("PostToolUse", "ai-tool");
    let event_path = fs::read_dir(
        malformed
            .home
            .join("repos")
            .join(malformed.repo_id())
            .join("events"),
    )
    .unwrap()
    .next()
    .unwrap()
    .unwrap()
    .path();
    let mut output = Vec::new();
    for line in fs::read(&event_path).unwrap().split(|byte| *byte == b'\n') {
        if line.is_empty() {
            continue;
        }
        let mut value: Value = serde_json::from_slice(line).unwrap();
        if value["event"] == "ai_change" && value["path"] == "tracked.txt" {
            value["after_sha"] = "../../outside".into();
        }
        serde_json::to_writer(&mut output, &value).unwrap();
        output.push(b'\n');
    }
    fs::write(event_path, output).unwrap();
    let commit = malformed.commit("bad event hash");
    assert!(derive(&malformed, &commit).status.success());
    let value = malformed.derived_value(&commit);
    assert_eq!(file(&value, "tracked.txt")["evidence"], "missing_blob");
    assert_eq!(file(&value, "tracked.txt")["unknown"], 1);
    assert_eq!(value["totals"]["unreadable_files"], 1);
    malformed.drop_root();
}

#[test]
fn skipped_binary_secret_deleted_and_renamed_paths_are_safe() {
    let fixture = Fixture::new("skipped-paths");
    fs::write(fixture.repo.join("credentials.txt"), b"new secret\n").unwrap();
    fs::write(fixture.repo.join("data.bin"), [b'b', 0, b'x', b'\n']).unwrap();
    fs::write(fixture.repo.join("large.txt"), vec![b'x'; 1024 * 1024 + 1]).unwrap();
    fs::write(fixture.repo.join("to-delete.txt"), b"old content\n").unwrap();
    let added_commit = fixture.commit("add files");
    assert!(derive(&fixture, &added_commit).status.success());
    let added_value = fixture.derived_value(&added_commit);
    assert_eq!(
        file(&added_value, "credentials.txt")["evidence"],
        "skipped_excluded"
    );
    assert_eq!(file(&added_value, "credentials.txt")["excluded"], 1);
    assert_eq!(file(&added_value, "data.bin")["evidence"], "skipped_binary");
    assert_eq!(file(&added_value, "large.txt")["evidence"], "skipped_large");
    assert_eq!(file(&added_value, "large.txt")["excluded"], 1);

    git(&fixture.repo, &["mv", "tracked.txt", "renamed.txt"]);
    fs::remove_file(fixture.repo.join("to-delete.txt")).unwrap();
    let commit = fixture.commit("rename and remove");
    assert!(derive(&fixture, &commit).status.success());
    let value = fixture.derived_value(&commit);
    assert!(value["files"]
        .as_array()
        .unwrap()
        .iter()
        .all(|entry| entry["path"] != "to-delete.txt"));
    assert_eq!(file(&value, "renamed.txt")["unknown"], 2);
    fixture.drop_root();
}

#[test]
fn symlinks_are_not_counted_as_added_text() {
    let fixture = Fixture::new("symlink");
    let target = git_blob(&fixture.repo, b"tracked.txt");
    let cacheinfo = format!("120000,{target},link.txt");
    git(
        &fixture.repo,
        &["update-index", "--add", "--cacheinfo", &cacheinfo],
    );
    git(&fixture.repo, &["commit", "-m", "add symlink"]);
    let commit = git_text(&fixture.repo, &["rev-parse", "HEAD"]);
    assert!(derive(&fixture, &commit).status.success());
    let value = fixture.derived_value(&commit);
    assert!(value["files"]
        .as_array()
        .unwrap()
        .iter()
        .all(|entry| entry["path"] != "link.txt"));
    fixture.drop_root();
}

#[test]
fn derive_show_range_and_worktree_are_deterministic() {
    let fixture = Fixture::new("deterministic-range");
    let parent = git_text(&fixture.repo, &["rev-parse", "HEAD"]);
    let before = git_text(&fixture.repo, &["status", "--porcelain"]);
    fs::write(fixture.repo.join("tracked.txt"), b"one\nnext\n").unwrap();
    let first = fixture.commit("first change");
    fs::write(fixture.repo.join("tracked.txt"), b"one\nlast\n").unwrap();
    let second = fixture.commit("second change");

    let derived_once = derive(&fixture, &second);
    assert!(derived_once.status.success());
    let first_bytes = fs::read(fixture.derived_path(&second)).unwrap();
    let first_lines_bytes = fs::read(fixture.lines_path(&second)).unwrap();
    let derived_twice = derive(&fixture, &second);
    assert!(derived_twice.status.success());
    assert_eq!(
        first_bytes,
        fs::read(fixture.derived_path(&second)).unwrap()
    );
    assert_eq!(
        first_lines_bytes,
        fs::read(fixture.lines_path(&second)).unwrap()
    );

    let value = fixture.derived_value(&second);
    assert_counts_consistent(&value);
    let shown = fixture.cli(&[
        "show",
        "--repo",
        fixture.repo.to_str().unwrap(),
        "--commit",
        &second,
        "--json",
    ]);
    assert!(shown.status.success());
    assert_eq!(shown.stdout, first_bytes);
    let human = fixture.cli(&[
        "show",
        "--repo",
        fixture.repo.to_str().unwrap(),
        "--commit",
        &second,
    ]);
    assert!(human.status.success());
    assert!(String::from_utf8_lossy(&human.stdout).contains("coverage"));

    let range = format!("{parent}..{second}");
    let ranged = fixture.cli(&[
        "derive",
        "--repo",
        fixture.repo.to_str().unwrap(),
        "--range",
        &range,
    ]);
    assert!(
        ranged.status.success(),
        "{}",
        String::from_utf8_lossy(&ranged.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&ranged.stdout).lines().count(), 2);
    assert!(fixture.derived_path(&first).exists());
    assert_eq!(before, git_text(&fixture.repo, &["status", "--porcelain"]));
    fixture.drop_root();
}

#[test]
fn show_missing_commit_fails_on_stderr() {
    let fixture = Fixture::new("show-missing");
    let result = fixture.cli(&[
        "show",
        "--repo",
        fixture.repo.to_str().unwrap(),
        "--commit",
        "HEAD",
    ]);
    assert!(!result.status.success());
    assert!(result.stdout.is_empty());
    assert!(String::from_utf8_lossy(&result.stderr).contains("has not been derived"));
    fixture.drop_root();
}

#[test]
fn unknown_and_missing_subcommands_keep_silent_hook_behavior() {
    for args in [Vec::<&str>::new(), vec!["unknownarg"]] {
        let output = isolated_command(env!("CARGO_BIN_EXE_prov-hook"))
            .args(args)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(0));
        assert!(output.stdout.is_empty());
        assert!(output.stderr.is_empty());
    }
}

#[test]
fn merge_commits_write_skip_record_and_empty_sidecar() {
    let fixture = Fixture::new("merge-commit");
    let base_branch = git_text(&fixture.repo, &["branch", "--show-current"]);
    git(&fixture.repo, &["checkout", "-b", "side"]);
    fs::write(fixture.repo.join("side.txt"), b"side\n").unwrap();
    let _ = fixture.commit("side change");
    git(&fixture.repo, &["checkout", &base_branch]);
    fs::write(fixture.repo.join("main.txt"), b"main\n").unwrap();
    let _ = fixture.commit("main change");
    git(
        &fixture.repo,
        &["merge", "--no-ff", "side", "-m", "merge side"],
    );
    let merge = git_text(&fixture.repo, &["rev-parse", "HEAD"]);

    assert!(derive(&fixture, &merge).status.success());
    let value = fixture.derived_value(&merge);
    assert_eq!(value["skipped"], "merge_commit");
    assert_eq!(value["patch_id"], Value::Null);
    assert_eq!(value["rewrite"]["method"], "none");
    assert_eq!(value["rewrite"]["sources"], json!([]));
    assert_eq!(value.as_object().unwrap().len(), 5);
    let lines_path = fixture
        .derived_path(&merge)
        .with_file_name(format!("{merge}.lines.json"));
    let lines: Value = serde_json::from_slice(&fs::read(lines_path).unwrap()).unwrap();
    assert_eq!(lines["schema"], "prov.lines/0.1");
    assert_eq!(lines["files"].as_array().unwrap().len(), 0);
    assert_eq!(lines["totals"]["e"], 0);
    fixture.drop_root();
}

#[test]
fn sidecar_hashes_added_nonblank_lines_and_matches_summary_counts() {
    let fixture = Fixture::new("line-sidecar");
    let secret = "API_KEY=sk-live-never-store-this-line";
    fixture.hook("PreToolUse", "sidecar-ai");
    fs::write(
        fixture.repo.join("tracked.txt"),
        format!("one\n{secret}\n\n"),
    )
    .unwrap();
    fixture.hook("PostToolUse", "sidecar-ai");
    let commit = fixture.commit("sidecar test");
    assert!(derive(&fixture, &commit).status.success());

    let summary = fixture.derived_value(&commit);
    let sidecar_bytes = fs::read(fixture.lines_path(&commit)).unwrap();
    assert!(!String::from_utf8_lossy(&sidecar_bytes).contains(secret));
    let sidecar: Value = serde_json::from_slice(&sidecar_bytes).unwrap();
    assert_eq!(sidecar["schema"], "prov.lines/0.1");
    for key in ["e", "ai_strong", "ai_probable", "mixed", "unknown"] {
        assert_eq!(sidecar["totals"][key], summary["totals"][key]);
    }
    let file_lines = sidecar["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|file| file["path"] == "tracked.txt")
        .unwrap()["lines"]
        .as_array()
        .unwrap();
    assert_eq!(file_lines.len(), 1);
    assert_eq!(file_lines[0]["line"], 2);
    assert_eq!(file_lines[0]["label"], "s");
    let expected_hash = format!("{:x}", Sha256::digest(secret.as_bytes()));
    assert_eq!(file_lines[0]["hash"], &expected_hash[..16]);
    fixture.drop_root();
}

#[test]
fn patch_id_matches_git_and_identical_patches_match_each_other() {
    let fixture = Fixture::new("patch-id");
    let base = git_text(&fixture.repo, &["rev-parse", "HEAD"]);
    git(&fixture.repo, &["checkout", "-b", "patch-one"]);
    fs::write(fixture.repo.join("same.txt"), b"same patch\n").unwrap();
    let first = fixture.commit("first identical patch");
    let expected = patch_id(&fixture.repo, &base, &first);
    assert!(derive(&fixture, &first).status.success());
    let first_id = fixture.derived_value(&first)["patch_id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(first_id, expected);

    git(&fixture.repo, &["checkout", &base]);
    git(&fixture.repo, &["checkout", "-b", "patch-two"]);
    fs::write(fixture.repo.join("same.txt"), b"same patch\n").unwrap();
    let second = fixture.commit("second identical patch");
    assert!(derive(&fixture, &second).status.success());
    let second_value = fixture.derived_value(&second);
    let second_id = second_value["patch_id"].as_str().unwrap();
    assert_eq!(second_id, first_id);
    fixture.drop_root();
}

#[test]
fn head_move_spawns_silent_pending_derivation_under_watchdog() {
    let fixture = Fixture::new("auto-derive");
    fixture.hook("PreToolUse", "auto-derive");
    fs::write(fixture.repo.join("tracked.txt"), b"one\nautomatic report\n").unwrap();
    let commit = fixture.commit("commit during AI session");

    let elapsed = fixture.hook_with_autoderive("PostToolUse", "auto-derive", true);
    assert!(
        elapsed < Duration::from_millis(1400),
        "hook took {elapsed:?}"
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline && !fixture.derived_path(&commit).exists() {
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(fixture.derived_path(&commit).exists());
    assert!(fixture.lines_path(&commit).exists());
    let summary = fixture.derived_value(&commit);
    let tracked = summary["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|file| file["path"] == "tracked.txt")
        .unwrap();
    assert!(
        tracked["ai_strong"].as_u64().unwrap() > 0 || tracked["ai_probable"].as_u64().unwrap() > 0
    );
    let sidecar: Value =
        serde_json::from_slice(&fs::read(fixture.lines_path(&commit)).unwrap()).unwrap();
    for key in ["e", "ai_strong", "ai_probable", "mixed", "unknown"] {
        assert_eq!(sidecar["totals"][key], summary["totals"][key]);
    }
    fixture.drop_root();
}

#[test]
fn pending_lock_skips_fresh_lock_takes_over_stale_lock_and_cleans_up() {
    let fixture = Fixture::new("pending-lock");
    fixture.hook("Stop", "pending-init");
    let commits = fixture.commits(1);
    fixture.queue_head_moves(&commits);
    let repo_store = fixture.home.join("repos").join(fixture.repo_id());
    let lock = repo_store.join("derive.lock");
    fs::write(&lock, "held").unwrap();

    let fresh = fixture.pending();
    assert_eq!(fresh.status.code(), Some(0));
    assert!(fresh.stdout.is_empty() && fresh.stderr.is_empty());
    assert!(!fixture.derived_path(&commits[0]).exists());

    set_age(&lock, Duration::from_secs(121));
    let stale = fixture.pending();
    assert_eq!(stale.status.code(), Some(0));
    assert!(stale.stdout.is_empty() && stale.stderr.is_empty());
    assert!(fixture.derived_path(&commits[0]).exists());
    assert!(!lock.exists());
    fixture.drop_root();
}

#[test]
fn pending_attempt_marker_skips_fresh_and_retries_stale_marker() {
    let fixture = Fixture::new("pending-attempt");
    fixture.hook("Stop", "pending-init");
    let commits = fixture.commits(1);
    fixture.queue_head_moves(&commits);
    let commits_dir = fixture
        .home
        .join("repos")
        .join(fixture.repo_id())
        .join("commits");
    fs::create_dir_all(&commits_dir).unwrap();
    let marker = commits_dir.join(format!("{}.attempt", commits[0]));
    fs::write(&marker, "attempting").unwrap();

    let fresh = fixture.pending();
    assert_eq!(fresh.status.code(), Some(0));
    assert!(!fixture.derived_path(&commits[0]).exists());

    set_age(&marker, Duration::from_secs(61));
    let stale = fixture.pending();
    assert_eq!(stale.status.code(), Some(0));
    assert!(fixture.derived_path(&commits[0]).exists());
    assert!(!marker.exists());
    fixture.drop_root();
}

#[test]
fn pending_run_removes_lock_after_derivation_error() {
    let fixture = Fixture::new("pending-error-cleanup");
    fixture.hook("Stop", "pending-init");
    let commits = fixture.commits(2);
    fixture.queue_head_moves(&commits);
    let repo_store = fixture.home.join("repos").join(fixture.repo_id());
    let commits_dir = repo_store.join("commits");
    fs::create_dir_all(commits_dir.join(format!("{}.lines.json", commits[0]))).unwrap();

    let result = fixture.pending();
    assert_eq!(result.status.code(), Some(0));
    assert!(result.stdout.is_empty() && result.stderr.is_empty());
    assert!(!repo_store.join("derive.lock").exists());
    assert!(!fixture.derived_path(&commits[0]).exists());
    assert!(commits_dir.join(format!("{}.attempt", commits[0])).exists());
    assert!(fixture.derived_path(&commits[1]).exists());
    fixture.drop_root();
}

#[test]
fn pending_skips_dead_commit_and_continues_then_doctor_reports_zero() {
    let fixture = Fixture::new("pending-dead-commit");
    fixture.hook("Stop", "pending-init");
    let real_commits = fixture.commits(2);
    let dead_commit = "f".repeat(40);
    let mut queued = vec![dead_commit.clone()];
    queued.extend(real_commits.clone());
    fixture.queue_head_moves(&queued);

    let first = fixture.pending();
    assert_eq!(first.status.code(), Some(0));
    assert!(first.stdout.is_empty() && first.stderr.is_empty());
    assert!(fixture
        .home
        .join("repos")
        .join(fixture.repo_id())
        .join("commits")
        .join(format!("{dead_commit}.gone"))
        .is_file());
    for commit in &real_commits {
        assert!(fixture.derived_path(commit).is_file());
    }

    let second = fixture.pending();
    assert_eq!(second.status.code(), Some(0));
    assert!(second.stdout.is_empty() && second.stderr.is_empty());
    let doctor = fixture.doctor();
    assert!(doctor.status.success());
    assert!(
        String::from_utf8_lossy(&doctor.stdout).contains("PASS pending commits: 0 not yet derived")
    );
    fixture.drop_root();
}

#[test]
fn pending_derivation_caps_at_fifty_and_continues_next_run() {
    let fixture = Fixture::new("pending-cap");
    fixture.hook("Stop", "pending-init");
    let commits = fixture.empty_commits(60);
    fixture.queue_head_moves(&commits);
    let first = fixture.pending();
    assert_eq!(first.status.code(), Some(0));
    assert!(first.stdout.is_empty() && first.stderr.is_empty());
    let commits_dir = fixture
        .home
        .join("repos")
        .join(fixture.repo_id())
        .join("commits");
    let count_derived = || {
        fs::read_dir(&commits_dir)
            .unwrap()
            .flatten()
            .filter(|entry| {
                entry
                    .path()
                    .extension()
                    .is_some_and(|extension| extension == "json")
                    && !entry.file_name().to_string_lossy().ends_with(".lines.json")
            })
            .count()
    };
    assert_eq!(count_derived(), 50);

    let second = fixture.pending();
    assert_eq!(second.status.code(), Some(0));
    assert_eq!(count_derived(), 60);
    fixture.drop_root();
}

#[test]
fn amend_message_carries_strong_labels_by_patch_id_and_pending_is_deterministic() {
    let fixture = Fixture::new("rewrite-amend-message");
    let phrase = "AI-authored persistent phrase";
    let source = ai_commit(
        &fixture,
        &format!("one\ntwo\n{phrase}\n"),
        "AI source commit",
    );
    assert_eq!(
        file(&fixture.derived_value(&source), "tracked.txt")["ai_strong"],
        1
    );
    fixture.clear_events();
    git(
        &fixture.repo,
        &["commit", "--amend", "-m", "amended message only"],
    );
    let amended = git_text(&fixture.repo, &["rev-parse", "HEAD"]);
    fixture.queue_head_moves(std::slice::from_ref(&amended));

    let pending = fixture.pending();
    assert_eq!(pending.status.code(), Some(0));
    assert!(pending.stdout.is_empty() && pending.stderr.is_empty());
    let report = fixture.derived_value(&amended);
    let sidecar = fixture.sidecar_value(&amended);
    assert_eq!(report["rewrite"]["method"], "patch_id");
    assert_eq!(report["rewrite"]["sources"], json!([source]));
    assert_eq!(report["rewrite"]["carried_lines"], 1);
    assert_eq!(line_label(&sidecar, "tracked.txt", &line_hash(phrase)), "s");
    assert_sidecar_counts_match(&report, &sidecar);
    assert!(
        !String::from_utf8_lossy(&fs::read(fixture.lines_path(&amended)).unwrap()).contains(phrase)
    );

    let report_bytes = fs::read(fixture.derived_path(&amended)).unwrap();
    let sidecar_bytes = fs::read(fixture.lines_path(&amended)).unwrap();
    assert!(derive(&fixture, &amended).status.success());
    assert_eq!(
        fs::read(fixture.derived_path(&amended)).unwrap(),
        report_bytes
    );
    assert_eq!(
        fs::read(fixture.lines_path(&amended)).unwrap(),
        sidecar_bytes
    );
    fixture.drop_root();
}

#[test]
fn cherry_picked_rebase_carries_strong_labels_by_patch_id() {
    let fixture = Fixture::new("rewrite-rebase");
    let base = git_text(&fixture.repo, &["rev-parse", "HEAD"]);
    let phrase = "AI-authored rebased phrase";
    git(&fixture.repo, &["checkout", "-b", "ai-source"]);
    let source = ai_commit(
        &fixture,
        &format!("one\ntwo\n{phrase}\n"),
        "AI source for rebase",
    );

    git(&fixture.repo, &["checkout", &base]);
    git(&fixture.repo, &["checkout", "-b", "rebased-base"]);
    fs::write(fixture.repo.join("other.txt"), b"different base\n").unwrap();
    let _ = fixture.commit("different base commit");
    git(&fixture.repo, &["cherry-pick", &source]);
    let rewritten = git_text(&fixture.repo, &["rev-parse", "HEAD"]);
    assert!(derive(&fixture, &rewritten).status.success());

    let report = fixture.derived_value(&rewritten);
    let sidecar = fixture.sidecar_value(&rewritten);
    assert_eq!(report["rewrite"]["method"], "patch_id");
    assert_eq!(report["rewrite"]["sources"], json!([source]));
    assert_eq!(line_label(&sidecar, "tracked.txt", &line_hash(phrase)), "s");
    assert_sidecar_counts_match(&report, &sidecar);
    fixture.drop_root();
}

#[test]
fn changed_amend_uses_content_and_caps_inherited_labels_at_probable() {
    let fixture = Fixture::new("rewrite-amend-content");
    let persistent = "persistent AI-authored phrase";
    let replaced = "old AI-authored phrase";
    let replacement = "new replacement wording";
    let source = ai_commit(
        &fixture,
        &format!("one\ntwo\n{persistent}\n{replaced}\n"),
        "AI source before edited amend",
    );
    fixture.clear_events();
    fs::write(
        fixture.repo.join("tracked.txt"),
        format!("one\ntwo\n{persistent}\n{replacement}\n"),
    )
    .unwrap();
    git(&fixture.repo, &["add", "tracked.txt"]);
    git(
        &fixture.repo,
        &["commit", "--amend", "-m", "amend with one edit"],
    );
    let amended = git_text(&fixture.repo, &["rev-parse", "HEAD"]);
    assert!(derive(&fixture, &amended).status.success());

    let report = fixture.derived_value(&amended);
    let sidecar = fixture.sidecar_value(&amended);
    assert_eq!(report["rewrite"]["method"], "content");
    assert_eq!(report["rewrite"]["sources"], json!([source]));
    assert_eq!(report["rewrite"]["carried_lines"], 1);
    assert_eq!(
        line_label(&sidecar, "tracked.txt", &line_hash(persistent)),
        "p"
    );
    assert_eq!(
        line_label(&sidecar, "tracked.txt", &line_hash(replacement)),
        "u"
    );
    assert_sidecar_counts_match(&report, &sidecar);
    fixture.drop_root();
}

#[test]
fn soft_reset_squash_carries_lines_from_both_derived_sources() {
    let fixture = Fixture::new("rewrite-squash");
    let base = git_text(&fixture.repo, &["rev-parse", "HEAD"]);
    let first_line = "first source AI phrase";
    let second_line = "second source AI phrase";
    let first = ai_commit(
        &fixture,
        &format!("one\ntwo\n{first_line}\n"),
        "first AI source",
    );
    let second = ai_commit(
        &fixture,
        &format!("one\ntwo\n{first_line}\n{second_line}\n"),
        "second AI source",
    );
    fixture.clear_events();
    git(&fixture.repo, &["reset", "--soft", &base]);
    let squashed = fixture.commit("squashed AI edits");
    assert!(derive(&fixture, &squashed).status.success());

    let report = fixture.derived_value(&squashed);
    let sidecar = fixture.sidecar_value(&squashed);
    let mut expected_sources = vec![first, second];
    expected_sources.sort();
    assert_eq!(report["rewrite"]["method"], "content");
    assert_eq!(report["rewrite"]["sources"], json!(expected_sources));
    assert_eq!(report["rewrite"]["carried_lines"], 2);
    assert_eq!(
        line_label(&sidecar, "tracked.txt", &line_hash(first_line)),
        "p"
    );
    assert_eq!(
        line_label(&sidecar, "tracked.txt", &line_hash(second_line)),
        "p"
    );
    assert_sidecar_counts_match(&report, &sidecar);
    let serialized = String::from_utf8(fs::read(fixture.lines_path(&squashed)).unwrap()).unwrap();
    assert!(!serialized.contains(first_line));
    assert!(!serialized.contains(second_line));
    fixture.drop_root();
}

#[test]
fn ancestor_sources_and_short_content_lines_are_not_carried() {
    let ancestor = Fixture::new("rewrite-ancestor");
    let shared = "long identical ancestor phrase";
    let source = ai_commit(
        &ancestor,
        &format!("one\ntwo\n{shared}\n"),
        "ancestor source",
    );
    ancestor.clear_events();
    fs::write(
        ancestor.repo.join("tracked.txt"),
        format!("one\ntwo\n{shared}\n{shared}\nnew target phrase\n"),
    )
    .unwrap();
    let target = ancestor.commit("descendant with duplicate line");
    assert!(derive(&ancestor, &target).status.success());
    let report = ancestor.derived_value(&target);
    assert_eq!(report["rewrite"]["method"], "none");
    assert_eq!(report["rewrite"]["carried_lines"], 0);
    assert_ne!(target, source);
    ancestor.drop_root();

    let short = Fixture::new("rewrite-short");
    let base = git_text(&short.repo, &["rev-parse", "HEAD"]);
    git(&short.repo, &["checkout", "-b", "short-source"]);
    let source = ai_commit(
        &short,
        "one\ntwo\n}\npass\nsource-only phrase\n",
        "short source",
    );
    git(&short.repo, &["checkout", &base]);
    short.clear_events();
    fs::write(
        short.repo.join("tracked.txt"),
        "one\ntwo\n}\npass\ntarget-only phrase\n",
    )
    .unwrap();
    let target = short.commit("short target");
    assert!(derive(&short, &target).status.success());
    let report = short.derived_value(&target);
    let sidecar = short.sidecar_value(&target);
    assert_eq!(report["rewrite"]["method"], "content");
    assert_eq!(report["rewrite"]["carried_lines"], 0);
    assert_eq!(report["rewrite"]["sources"], json!([]));
    assert_eq!(line_label(&sidecar, "tracked.txt", &line_hash("}")), "u");
    assert_eq!(line_label(&sidecar, "tracked.txt", &line_hash("pass")), "u");
    assert_ne!(target, source);
    short.drop_root();
}

#[test]
fn conflicting_content_sources_choose_the_weakest_label() {
    let fixture = Fixture::new("rewrite-conflict");
    let base = git_text(&fixture.repo, &["rev-parse", "HEAD"]);
    let shared = "long shared conflicting phrase";
    git(&fixture.repo, &["checkout", "-b", "unknown-source"]);
    fs::write(
        fixture.repo.join("tracked.txt"),
        format!("one\ntwo\n{shared}\n"),
    )
    .unwrap();
    let unknown = fixture.commit("unknown source");
    assert!(derive(&fixture, &unknown).status.success());
    assert_eq!(
        file(&fixture.derived_value(&unknown), "tracked.txt")["unknown"],
        1
    );

    git(&fixture.repo, &["checkout", &base]);
    git(&fixture.repo, &["checkout", "-b", "strong-source"]);
    let strong = ai_commit(&fixture, &format!("one\ntwo\n{shared}\n"), "strong source");
    git(&fixture.repo, &["checkout", &base]);
    fixture.clear_events();
    fs::write(
        fixture.repo.join("tracked.txt"),
        format!("one\ntwo\n{shared}\ntarget-only phrase\n"),
    )
    .unwrap();
    let target = fixture.commit("conflicting target");
    assert!(derive(&fixture, &target).status.success());
    let report = fixture.derived_value(&target);
    let sidecar = fixture.sidecar_value(&target);
    let mut expected_sources = vec![strong.clone(), unknown.clone()];
    expected_sources.sort();
    assert_eq!(report["rewrite"]["method"], "content");
    assert_eq!(report["rewrite"]["carried_lines"], 0);
    assert_eq!(line_label(&sidecar, "tracked.txt", &line_hash(shared)), "u");
    assert_eq!(report["rewrite"]["sources"], json!(expected_sources));
    assert_ne!(strong, unknown);
    fixture.drop_root();
}

#[test]
fn rewrite_hash_entry_limit_keeps_the_normal_report() {
    let fixture = Fixture::new("rewrite-entry-limit");
    let source = ai_commit(
        &fixture,
        "one\ntwo\nAI source phrase for entry limit\n",
        "large source",
    );
    let source_sidecar_path = fixture.lines_path(&source);
    let source_sidecar = fixture.sidecar_value(&source);
    let hash = source_sidecar["files"][0]["lines"][0]["hash"]
        .as_str()
        .unwrap()
        .to_owned();
    let entries: Vec<_> = (1..=200_001)
        .map(|line| json!({"line":line,"hash":hash,"label":"s"}))
        .collect();
    let oversized = json!({
        "schema":"prov.lines/0.1",
        "repo_id":source_sidecar["repo_id"],
        "commit":source,
        "totals":{"e":200001,"ai_strong":200001,"ai_probable":0,"mixed":0,"unknown":0},
        "files":[{"path":"tracked.txt","lines":entries}],
    });
    fs::write(
        &source_sidecar_path,
        serde_json::to_vec(&oversized).unwrap(),
    )
    .unwrap();
    fixture.clear_events();
    fs::write(
        fixture.repo.join("tracked.txt"),
        "one\ntwo\nindependent target phrase\n",
    )
    .unwrap();
    let target = fixture.commit("target after entry cap");
    assert!(derive(&fixture, &target).status.success());
    let report = fixture.derived_value(&target);
    assert_eq!(report["rewrite"]["method"], "none");
    assert_eq!(report["rewrite"]["carried_lines"], 0);
    assert_eq!(file(&report, "tracked.txt")["unknown"], 1);
    assert_sidecar_counts_match(&report, &fixture.sidecar_value(&target));
    fixture.drop_root();
}
