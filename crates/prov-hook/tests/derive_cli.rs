use serde_json::{json, Value};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

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
        let payload = json!({
            "hook_event_name": event,
            "session_id": "derive-test-session",
            "turn_id": "derive-test-turn",
            "cwd": self.repo,
            "tool_use_id": tool,
            "tool_name": "PowerShell",
            "tool_input": {"command": "test command"},
        });
        let mut child = Command::new(env!("CARGO_BIN_EXE_prov-hook"))
            .arg("codex")
            .env("PROV_HOME", &self.home)
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
        let output = child.wait_with_output().unwrap();
        assert_eq!(output.status.code(), Some(0));
        assert!(output.stdout.is_empty());
        assert!(output.stderr.is_empty());
    }

    fn cli(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_prov-hook"))
            .args(args)
            .env("PROV_HOME", &self.home)
            .output()
            .unwrap()
    }

    fn derive_with_git_trace(&self, commit: &str, trace: &Path) -> Output {
        Command::new(env!("CARGO_BIN_EXE_prov-hook"))
            .args([
                "derive",
                "--repo",
                self.repo.to_str().unwrap(),
                "--commit",
                commit,
            ])
            .env("PROV_HOME", &self.home)
            .env("GIT_TRACE", trace)
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

    fn derived_value(&self, commit: &str) -> Value {
        serde_json::from_slice(&fs::read(self.derived_path(commit)).unwrap()).unwrap()
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

fn git(repo: &Path, args: &[&str]) {
    let output = Command::new("git")
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
    let output = Command::new("git")
        .args(args)
        .current_dir(repo)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?} failed");
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn git_blob(repo: &Path, bytes: &[u8]) -> String {
    let mut child = Command::new("git")
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
    assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
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
    let trace_path = fixture.root.join("large-file-git-trace.log");
    assert!(fixture
        .derive_with_git_trace(&added_commit, &trace_path)
        .status
        .success());
    let trace = fs::read_to_string(trace_path).unwrap();
    assert!(trace.contains("cat-file --batch-check"));
    assert!(!trace
        .lines()
        .any(|line| line.contains("cat-file --batch\"") || line.contains("cat-file --batch ")));
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
    let derived_twice = derive(&fixture, &second);
    assert!(derived_twice.status.success());
    assert_eq!(
        first_bytes,
        fs::read(fixture.derived_path(&second)).unwrap()
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
        let output = Command::new(env!("CARGO_BIN_EXE_prov-hook"))
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
fn merge_commits_write_only_the_skip_record() {
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
    assert_eq!(value.as_object().unwrap().len(), 3);
    fixture.drop_root();
}
