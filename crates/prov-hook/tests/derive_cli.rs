use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::cell::RefCell;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Mutex, MutexGuard};
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

static FIXTURE_TEST_GATE: Mutex<()> = Mutex::new(());

thread_local! {
    static FIXTURE_TEST_GATE_STATE: RefCell<(usize, Option<MutexGuard<'static, ()>>)> = const { RefCell::new((0, None)) };
}

struct FixtureTestGuard;

impl FixtureTestGuard {
    fn new() -> Self {
        FIXTURE_TEST_GATE_STATE.with(|state| {
            let mut state = state.borrow_mut();
            if state.0 == 0 {
                state.1 = Some(
                    FIXTURE_TEST_GATE
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner()),
                );
            }
            state.0 += 1;
        });
        Self
    }
}

impl Drop for FixtureTestGuard {
    fn drop(&mut self) {
        FIXTURE_TEST_GATE_STATE.with(|state| {
            let mut state = state.borrow_mut();
            state.0 -= 1;
            if state.0 == 0 {
                drop(state.1.take());
            }
        });
    }
}

struct Fixture {
    root: PathBuf,
    repo: PathBuf,
    home: PathBuf,
    _test_guard: FixtureTestGuard,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let test_guard = FixtureTestGuard::new();
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
        Self {
            root,
            repo,
            home,
            _test_guard: test_guard,
        }
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

    fn commit_at(&self, message: &str, date: &str) -> String {
        git(&self.repo, &["add", "-A"]);
        git_with_dates(&self.repo, &["commit", "-m", message], date);
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
        let base = git_text(&self.repo, &["rev-parse", "HEAD"]);
        let ref_name = format!("refs/heads/prov-empty-{}-{}", std::process::id(), now());
        let now_seconds = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let mut stream = String::new();
        for index in 0..count {
            let message = format!("empty pending {index}");
            let parent = if index == 0 {
                base.clone()
            } else {
                format!(":{index}")
            };
            stream.push_str(&format!(
                "commit {ref_name}\nmark :{}\ncommitter Derive Test <derive@example.invalid> {} +0000\ndata {}\n{}\nfrom {parent}\n\n",
                index + 1,
                now_seconds.saturating_sub((count - index) as u64),
                message.len(),
                message,
            ));
        }
        stream.push_str("done\n");
        let mut child = isolated_command("git")
            .args(["fast-import", "--quiet"])
            .current_dir(&self.repo)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(stream.as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "git fast-import failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let range = format!("{base}..{ref_name}");
        git_text(&self.repo, &["rev-list", "--reverse", &range])
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn remove_loose_tree_object(&self, commit: &str) -> PathBuf {
        let tree_revision = format!("{commit}^{{tree}}");
        let tree = git_text(&self.repo, &["rev-parse", &tree_revision]);
        let object = self
            .repo
            .join(".git/objects")
            .join(&tree[..2])
            .join(&tree[2..]);
        #[cfg(windows)]
        {
            let mut permissions = fs::metadata(&object).unwrap().permissions();
            #[allow(clippy::permissions_set_readonly_false)]
            permissions.set_readonly(false);
            fs::set_permissions(&object, permissions).unwrap();
        }
        fs::remove_file(&object).unwrap();
        let commit_revision = format!("{commit}^{{commit}}");
        assert!(isolated_command("git")
            .args(["cat-file", "-e", &commit_revision])
            .current_dir(&self.repo)
            .status()
            .unwrap()
            .success());
        object
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

fn commits_listing(directory: &Path) -> String {
    fs::read_dir(directory)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(", ")
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

fn git_with_dates(repo: &Path, args: &[&str], date: &str) {
    let output = isolated_command("git")
        .args(args)
        .current_dir(repo)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_AUTHOR_NAME", "Derive Test")
        .env("GIT_AUTHOR_EMAIL", "derive@example.invalid")
        .env("GIT_COMMITTER_NAME", "Derive Test")
        .env("GIT_COMMITTER_EMAIL", "derive@example.invalid")
        .env("GIT_AUTHOR_DATE", date)
        .env("GIT_COMMITTER_DATE", date)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(output.success(), "git {args:?} failed");
}

fn set_ai_event_timestamp(fixture: &Fixture, path: &str, timestamp: u128) {
    let event_dir = fixture
        .home
        .join("repos")
        .join(fixture.repo_id())
        .join("events");
    for entry in fs::read_dir(event_dir).unwrap().flatten() {
        let event_path = entry.path();
        let mut output = Vec::new();
        for line in fs::read(&event_path).unwrap().split(|byte| *byte == b'\n') {
            if line.is_empty() {
                continue;
            }
            let mut event: Value = serde_json::from_slice(line).unwrap();
            if event["event"] == "ai_change" && event["path"] == path {
                event["timestamp"] = json!(timestamp);
            }
            serde_json::to_writer(&mut output, &event).unwrap();
            output.push(b'\n');
        }
        fs::write(event_path, output).unwrap();
    }
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

fn explain_json(fixture: &Fixture, commit: &str, path: &str, line: Option<usize>) -> Value {
    let mut args = vec![
        "explain".to_owned(),
        "--repo".to_owned(),
        fixture.repo.to_string_lossy().into_owned(),
        "--commit".to_owned(),
        commit.to_owned(),
        "--file".to_owned(),
        path.to_owned(),
        "--format".to_owned(),
        "json".to_owned(),
    ];
    if let Some(line) = line {
        args.push("--line".to_owned());
        args.push(line.to_string());
    }
    let refs = args.iter().map(String::as_str).collect::<Vec<_>>();
    let output = fixture.cli(&refs);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn snapshot_store(fixture: &Fixture) -> Vec<(PathBuf, Vec<u8>)> {
    fn walk(root: &Path, current: &Path, files: &mut Vec<(PathBuf, Vec<u8>)>) {
        for entry in fs::read_dir(current).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(root, &path, files);
            } else {
                files.push((
                    path.strip_prefix(root).unwrap().to_path_buf(),
                    fs::read(path).unwrap(),
                ));
            }
        }
    }
    let mut files = Vec::new();
    if fixture.home.exists() {
        walk(&fixture.home, &fixture.home, &mut files);
    }
    files.sort_by(|left, right| left.0.cmp(&right.0));
    files
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

fn sidecar_value(fixture: &Fixture, commit: &str) -> Value {
    serde_json::from_slice(&fs::read(fixture.lines_path(commit)).unwrap()).unwrap()
}

fn attribution_for<'a>(sidecar: &'a Value, path: &str, category: &str) -> &'a Value {
    sidecar["attributions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|range| range["path"] == path && range["category"] == category)
        .unwrap_or_else(|| panic!("missing {category} attribution for {path}"))
}

fn set_ai_change_field(fixture: &Fixture, field: &str, value: Value) {
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
        let mut event: Value = serde_json::from_slice(line).unwrap();
        if event["event"] == "ai_change" && event["path"] == "tracked.txt" {
            if field == "overlap" && value == true {
                let open = event["window_open"].as_u64().unwrap();
                let close = event["window_close"].as_u64().unwrap();
                for (timestamp, event_name, opened, closed) in [
                    (open + 1, "window_open", Some(open + 1), None),
                    (close, "window_close", Some(open + 1), Some(close - 1)),
                ] {
                    let concurrent = json!({
                        "schema_version":"prov.event/0.1",
                        "event":event_name,
                        "timestamp":timestamp,
                        "session_id":"independent-overlap-session",
                        "tool_use_id":"independent-overlap-tool",
                        "window_open":opened,
                        "window_close":closed,
                    });
                    serde_json::to_writer(&mut output, &concurrent).unwrap();
                    output.push(b'\n');
                }
            }
            event[field] = value.clone();
        }
        serde_json::to_writer(&mut output, &event).unwrap();
        output.push(b'\n');
    }
    fs::write(event_path, output).unwrap();
}

fn set_fixture_apply_patch_field(fixture: &Fixture, path: &str, field: &str, value: Value) {
    let event_dir = fixture
        .home
        .join("repos")
        .join(fixture.repo_id())
        .join("events");
    for entry in fs::read_dir(event_dir).unwrap().flatten() {
        let event_path = entry.path();
        let mut output = Vec::new();
        for line in fs::read(&event_path).unwrap().split(|byte| *byte == b'\n') {
            if line.is_empty() {
                continue;
            }
            let mut event: Value = serde_json::from_slice(line).unwrap();
            if event["event"] == "ai_change" && event["path"] == path {
                event[field] = value.clone();
            }
            serde_json::to_writer(&mut output, &event).unwrap();
            output.push(b'\n');
        }
        fs::write(event_path, output).unwrap();
    }
}

fn configure_structured_apply_patch(
    fixture: &Fixture,
    path: &str,
    structured_edit: Option<bool>,
    overlap: bool,
    line_ranges: Value,
) {
    let event_dir = fixture
        .home
        .join("repos")
        .join(fixture.repo_id())
        .join("events");
    for entry in fs::read_dir(event_dir).unwrap().flatten() {
        let event_path = entry.path();
        let mut output = Vec::new();
        for line in fs::read(&event_path).unwrap().split(|byte| *byte == b'\n') {
            if line.is_empty() {
                continue;
            }
            let mut event: Value = serde_json::from_slice(line).unwrap();
            if event["event"] == "ai_change" && event["path"] == path {
                event["operation"] = if structured_edit == Some(false) {
                    json!("Bash")
                } else {
                    json!("apply_patch")
                };
                if let Some(value) = structured_edit {
                    event["structured_edit"] = json!(value);
                } else {
                    event.as_object_mut().unwrap().remove("structured_edit");
                }
                event["line_ranges"] = line_ranges.clone();
                event["overlap"] = json!(overlap);
                event["degraded"] = json!(false);
                event["after_incomplete"] = json!(false);
                event["command_sha256"] = json!("a".repeat(64));
                event["command_length"] = json!(82);
                event
                    .as_object_mut()
                    .unwrap()
                    .remove("structured_added_lines");
                event
                    .as_object_mut()
                    .unwrap()
                    .remove("structured_input_sha256");
            }
            serde_json::to_writer(&mut output, &event).unwrap();
            output.push(b'\n');
        }
        fs::write(event_path, output).unwrap();
    }
}

fn write_fixture_apply_patch_event(
    fixture: &Fixture,
    path: &str,
    before: &[u8],
    after: &[u8],
    line_ranges: Value,
) {
    let repo_id = fixture.repo_id();
    let event_dir = fixture.home.join("repos").join(&repo_id).join("events");
    let blobs = fixture.home.join("repos").join(&repo_id).join("blobs");
    fs::create_dir_all(&blobs).unwrap();
    let before_sha = format!("{:x}", Sha256::digest(before));
    let after_sha = format!("{:x}", Sha256::digest(after));
    fs::write(blobs.join(&before_sha), before).unwrap();
    fs::write(blobs.join(&after_sha), after).unwrap();

    let mut event: Value = serde_json::from_str(include_str!(
        "fixtures/real_apply_patch_existing_file/event.jsonl"
    ))
    .unwrap();
    event["repo_id"] = json!(repo_id);
    event["path"] = json!(path);
    event["before_sha"] = json!(before_sha);
    event["after_sha"] = json!(after_sha);
    event["line_ranges"] = line_ranges;
    event["timestamp"] = json!(now());
    event["tool_use_id"] = json!(format!("fixture-{}", now()));
    event["head"] = json!(git_text(&fixture.repo, &["rev-parse", "HEAD"]));
    event["overlap"] = json!(false);
    event["degraded"] = json!(false);
    event["after_incomplete"] = json!(false);
    fs::create_dir_all(&event_dir).unwrap();
    fs::write(
        event_dir.join("real-apply-patch.jsonl"),
        format!("{}\n", event),
    )
    .unwrap();
}

#[derive(Default)]
struct StructuredFixtureOptions<'a> {
    autocrlf: Option<&'a str>,
    corrupt_hash: Option<&'a str>,
    remove_after_blob: bool,
}

fn structured_existing_file_commit(
    name: &str,
    path: &str,
    before: &[u8],
    after: &[u8],
    line_ranges: Value,
) -> (Fixture, String, Value) {
    structured_existing_file_commit_with_options(
        name,
        path,
        before,
        after,
        line_ranges,
        StructuredFixtureOptions::default(),
    )
}

fn structured_existing_file_commit_with_options(
    name: &str,
    path: &str,
    before: &[u8],
    after: &[u8],
    line_ranges: Value,
    options: StructuredFixtureOptions<'_>,
) -> (Fixture, String, Value) {
    let fixture = Fixture::new(name);
    if let Some(value) = options.autocrlf {
        git(&fixture.repo, &["config", "core.autocrlf", value]);
    }
    let file_path = fixture.repo.join(path);
    fs::create_dir_all(file_path.parent().unwrap()).unwrap();
    fs::write(&file_path, before).unwrap();
    let _parent = fixture.commit("before structured apply_patch");
    fixture.hook("PreToolUse", "fixture-apply-patch");
    fixture.clear_events();
    fs::write(&file_path, after).unwrap();
    write_fixture_apply_patch_event(&fixture, path, before, after, line_ranges);
    if let Some(field) = options.corrupt_hash {
        let event_path = fixture
            .home
            .join("repos")
            .join(fixture.repo_id())
            .join("events")
            .join("real-apply-patch.jsonl");
        let mut event: Value = serde_json::from_slice(&fs::read(&event_path).unwrap()).unwrap();
        let sha = event[field].as_str().unwrap();
        let replacement = if sha.as_bytes()[0] == b'a' { "b" } else { "a" };
        event[field] = json!(format!("{replacement}{}", &sha[1..]));
        fs::write(&event_path, format!("{}\n", event)).unwrap();
    }
    if options.remove_after_blob {
        let after_sha = format!("{:x}", Sha256::digest(after));
        fs::remove_file(
            fixture
                .home
                .join("repos")
                .join(fixture.repo_id())
                .join("blobs")
                .join(after_sha),
        )
        .unwrap();
    }
    let commit = fixture.commit("structured apply_patch");
    let result = derive(&fixture, &commit);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let report = fixture.derived_value(&commit);
    (fixture, commit, report)
}

fn mark_event_external(fixture: &Fixture, tool_use_id: &str) {
    let event_dir = fixture
        .home
        .join("repos")
        .join(fixture.repo_id())
        .join("events");
    for entry in fs::read_dir(event_dir).unwrap().flatten() {
        let event_path = entry.path();
        let mut output = Vec::new();
        for line in fs::read(&event_path).unwrap().split(|byte| *byte == b'\n') {
            if line.is_empty() {
                continue;
            }
            let mut event: Value = serde_json::from_slice(line).unwrap();
            if event["event"] == "ai_change" && event["tool_use_id"] == tool_use_id {
                event["event"] = json!("external_change");
            }
            serde_json::to_writer(&mut output, &event).unwrap();
            output.push(b'\n');
        }
        fs::write(event_path, output).unwrap();
    }
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

const S_PY_AI_PARENT: &str =
    "def add(a,b):\n    return a + b\ndef sub(a,b):\n    return a - b\ndef neg(a):\n    return -a\n";

const PALINDROME_AI_SNAPSHOT: &str = "def is_palindrome(number):\n    \"\"\"Return whether the number reads the same backward.\"\"\"\n    if number < 0:\n        return False\n\n    digits = str(number)\n    return digits == digits[::-1]\n\n\nnumber = int(input(\"Enter a number: \"))\n\nif is_palindrome(number):\n    print(\"Palindrome\")\nelse:\n    print(\"Not a palindrome\")\n";

fn time_cut_new_file(fixture: &Fixture, final_contents: &str, message: &str) -> String {
    fixture.hook("PreToolUse", "new-file-codex");
    fs::write(fixture.repo.join("final.py"), PALINDROME_AI_SNAPSHOT).unwrap();
    fixture.hook("PostToolUse", "new-file-codex");
    fs::write(fixture.repo.join("final.py"), final_contents).unwrap();
    let commit = fixture.commit(message);
    let result = derive(fixture, &commit);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    commit
}

fn attribution_at_line<'a>(sidecar: &'a Value, path: &str, line: usize) -> &'a Value {
    sidecar["attributions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|range| {
            range["path"] == path
                && range["startLine"].as_u64().unwrap() <= line as u64
                && range["endLine"].as_u64().unwrap() >= line as u64
        })
        .unwrap_or_else(|| panic!("missing attribution for {path}:{line}"))
}

fn ai_s_python_parent(fixture: &Fixture) -> String {
    fixture.hook("PreToolUse", "ai-s-python");
    fs::write(fixture.repo.join("s.py"), S_PY_AI_PARENT).unwrap();
    fixture.hook("PostToolUse", "ai-s-python");
    let parent = fixture.commit("AI creates s.py");
    assert!(derive(fixture, &parent).status.success());
    let sidecar = fixture.sidecar_value(&parent);
    let lines = sidecar["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|file| file["path"] == "s.py")
        .unwrap()["lines"]
        .as_array()
        .unwrap();
    assert_eq!(lines.len(), 6);
    assert!(lines.iter().all(|line| line["label"] == "s"));
    parent
}

fn silent_s_python_child(fixture: &Fixture, parent: &str, contents: &str, message: &str) -> String {
    git(&fixture.repo, &["checkout", parent]);
    fixture.clear_events();
    fs::write(fixture.repo.join("s.py"), contents).unwrap();
    let commit = fixture.commit(message);
    let result = derive(fixture, &commit);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    commit
}

fn mark_parent_s_python_ranges_probable(fixture: &Fixture, parent: &str) {
    let mut sidecar = fixture.sidecar_value(parent);
    for file in sidecar["files"].as_array_mut().unwrap() {
        if file["path"] == "s.py" {
            for line in file["lines"].as_array_mut().unwrap() {
                line["label"] = json!("p");
            }
        }
    }
    for range in sidecar["attributions"].as_array_mut().unwrap() {
        if range["path"] == "s.py" {
            range["category"] = json!("ai_probable");
            range["score"] = json!(0.79);
            range["confidenceBand"] = json!("medium");
        }
    }
    fs::write(
        fixture.lines_path(parent),
        serde_json::to_vec(&sidecar).unwrap(),
    )
    .unwrap();
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
    assert_eq!(value["coverage"], "low");
    human.drop_root();
}

#[test]
fn no_event_parent_lineage_fallback_handles_the_s_python_repro() {
    let fixture = Fixture::new("parent-lineage-s-python");
    let parent = ai_s_python_parent(&fixture);
    let child = silent_s_python_child(
        &fixture,
        &parent,
        "def add(a,b):\n    return a + b\ndef sub(a,b):\n    return a - b-1\nprint(add(1,2))\n",
        "manual s.py edits without events",
    );

    let report = fixture.derived_value(&child);
    let s_python = file(&report, "s.py");
    assert_eq!(s_python["evidence"], "none");
    assert_eq!(s_python["e"], 2);
    assert_eq!(s_python["mixed"], 1);
    assert_eq!(s_python["unknown"], 1);
    assert_eq!(s_python["ai_strong"], 0);
    assert_eq!(report["coverage"], "low");

    let sidecar = fixture.sidecar_value(&child);
    let mixed = attribution_for(&sidecar, "s.py", "mixed");
    assert_eq!(mixed["startLine"], 4);
    assert_eq!(mixed["primaryBasis"], "CONFLICTING_EVIDENCE");
    assert_eq!(mixed["source"]["toolId"], "codex");
    assert_eq!(mixed["score"], 0.84);
    assert!(mixed["limitingBasis"]
        .as_array()
        .unwrap()
        .contains(&json!("X_UNOBSERVED_EDIT_IN_FILE")));
    let unknown = attribution_for(&sidecar, "s.py", "unknown");
    assert_eq!(unknown["startLine"], 5);
    assert_eq!(unknown["primaryBasis"], Value::Null);
    assert_eq!(unknown["score"], Value::Null);
    let child_lines = sidecar["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["path"] == "s.py")
        .unwrap()["lines"]
        .as_array()
        .unwrap();
    assert_eq!(child_lines.len(), 2);
    assert_eq!(
        child_lines
            .iter()
            .map(|line| line["label"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["m", "u"]
    );

    // The four unchanged lines remain strongly attributed in the parent record.
    let parent_sidecar = fixture.sidecar_value(&parent);
    let parent_lines = parent_sidecar["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["path"] == "s.py")
        .unwrap()["lines"]
        .as_array()
        .unwrap();
    assert_eq!(parent_lines.len(), 6);
    assert!(parent_lines.iter().all(|line| line["label"] == "s"));
    fixture.drop_root();
}

#[test]
fn no_event_parent_lineage_fallback_requires_a_similar_removed_ai_line() {
    let addition = Fixture::new("parent-lineage-pure-addition");
    let parent = ai_s_python_parent(&addition);
    let child = silent_s_python_child(
        &addition,
        &parent,
        &format!("{S_PY_AI_PARENT}print(add(1,2))\n"),
        "pure manual addition",
    );
    let report = addition.derived_value(&child);
    assert_eq!(file(&report, "s.py")["unknown"], 1);
    assert_eq!(file(&report, "s.py")["mixed"], 0);
    assert_eq!(report["coverage"], "low");
    addition.drop_root();

    let unrelated = Fixture::new("parent-lineage-unrelated-replacement");
    let parent = ai_s_python_parent(&unrelated);
    let child = silent_s_python_child(
        &unrelated,
        &parent,
        "def add(a,b):\n    return a + b\ndef sub(a,b):\n    orchard mountain\ndef neg(a):\n    return -a\n",
        "unrelated manual replacement",
    );
    let report = unrelated.derived_value(&child);
    assert_eq!(file(&report, "s.py")["unknown"], 1);
    assert_eq!(file(&report, "s.py")["mixed"], 0);
    unrelated.drop_root();
}

#[test]
fn no_event_parent_lineage_fallback_mixes_a_probable_parent_line() {
    let fixture = Fixture::new("parent-lineage-probable");
    let parent = ai_s_python_parent(&fixture);
    mark_parent_s_python_ranges_probable(&fixture, &parent);
    let child = silent_s_python_child(
        &fixture,
        &parent,
        "def add(a,b):\n    return a + b\ndef sub(a,b):\n    return a - b-1\ndef neg(a):\n    return -a\n",
        "replace probable parent line",
    );
    let report = fixture.derived_value(&child);
    assert_eq!(file(&report, "s.py")["mixed"], 1);
    assert_eq!(file(&report, "s.py")["unknown"], 0);
    let sidecar = fixture.sidecar_value(&child);
    let mixed = attribution_for(&sidecar, "s.py", "mixed");
    assert_eq!(mixed["score"], 0.84);
    assert_eq!(mixed["source"]["toolId"], "codex");
    fixture.drop_root();
}

#[test]
fn no_event_parent_lineage_without_parent_sidecar_stays_unknown() {
    let fixture = Fixture::new("parent-lineage-no-sidecar");
    let parent = ai_s_python_parent(&fixture);
    fs::remove_file(fixture.lines_path(&parent)).unwrap();
    let child = silent_s_python_child(
        &fixture,
        &parent,
        "def add(a,b):\n    return a + b\ndef sub(a,b):\n    return a - b-1\ndef neg(a):\n    return -a\n",
        "parent sidecar missing",
    );
    let report = fixture.derived_value(&child);
    assert_eq!(file(&report, "s.py")["unknown"], 1);
    assert_eq!(file(&report, "s.py")["mixed"], 0);
    fixture.drop_root();
}

#[test]
fn time_cut_new_file_similarity_replacement_is_mixed_but_new_line_is_unknown() {
    let fixture = Fixture::new("time-cut-new-file-mixed-replacement");
    let final_contents = PALINDROME_AI_SNAPSHOT.replace(
        "    print(\"Not a palindrome\")\n",
        "    print(\"Not a palindrome and also this is mixed\")\n    print(\"this line should be unknown\")\n",
    );
    let commit = time_cut_new_file(&fixture, &final_contents, "user edits new AI file");
    let report = fixture.derived_value(&commit);
    let final_file = file(&report, "final.py");
    assert_eq!(final_file["evidence"], "time_cut");
    assert_eq!(final_file["ai_strong"], 10);
    assert_eq!(final_file["mixed"], 1);
    assert_eq!(final_file["unknown"], 1);
    assert_eq!(report["coverage"], "low");

    let sidecar = fixture.sidecar_value(&commit);
    let mixed = attribution_at_line(&sidecar, "final.py", 15);
    assert_eq!(mixed["category"], "mixed");
    assert_eq!(mixed["source"]["toolId"], "codex");
    assert_eq!(mixed["score"], 0.84);
    assert!(mixed["limitingBasis"]
        .as_array()
        .unwrap()
        .contains(&json!("X_UNOBSERVED_EDIT_IN_FILE")));
    let unknown = attribution_at_line(&sidecar, "final.py", 16);
    assert_eq!(unknown["category"], "unknown");
    assert_eq!(unknown["primaryBasis"], Value::Null);
    assert_eq!(unknown["score"], Value::Null);
    assert!(unknown["limitingBasis"]
        .as_array()
        .unwrap()
        .contains(&json!("X_UNOBSERVED_EDIT")));
    for line in [1, 2, 3, 4, 6, 7, 10, 12, 13, 14] {
        assert_eq!(
            attribution_at_line(&sidecar, "final.py", line)["category"],
            "ai_strong"
        );
    }
    fixture.drop_root();
}

#[test]
fn time_cut_new_file_similarity_can_mix_two_replaced_ai_lines() {
    let fixture = Fixture::new("time-cut-new-file-two-mixed-replacements");
    let final_contents = PALINDROME_AI_SNAPSHOT
        .replace(
            "    return digits == digits[::-1]\n",
            "    return digits == digits[::-1] and True\n",
        )
        .replace(
            "    print(\"Not a palindrome\")\n",
            "    print(\"Not a palindrome and also this is mixed\")\n",
        );
    let commit = time_cut_new_file(&fixture, &final_contents, "user edits two AI lines");
    let report = fixture.derived_value(&commit);
    let final_file = file(&report, "final.py");
    assert_eq!(final_file["mixed"], 2);
    assert_eq!(final_file["unknown"], 0);
    assert_eq!(final_file["ai_strong"], 9);
    let sidecar = fixture.sidecar_value(&commit);
    assert_eq!(
        attribution_at_line(&sidecar, "final.py", 7)["category"],
        "mixed"
    );
    assert_eq!(
        attribution_at_line(&sidecar, "final.py", 15)["category"],
        "mixed"
    );
    assert_eq!(
        attribution_at_line(&sidecar, "final.py", 15)["source"]["toolId"],
        "codex"
    );
    fixture.drop_root();
}

#[test]
fn time_cut_new_file_unrelated_replacement_stays_unknown() {
    let fixture = Fixture::new("time-cut-new-file-unrelated-replacement");
    let final_contents = PALINDROME_AI_SNAPSHOT.replace(
        "    print(\"Not a palindrome\")\n",
        "    print(\"completely unrelated output\")\n",
    );
    let commit = time_cut_new_file(&fixture, &final_contents, "user replaces AI line");
    let report = fixture.derived_value(&commit);
    let final_file = file(&report, "final.py");
    assert_eq!(final_file["unknown"], 1);
    assert_eq!(final_file["mixed"], 0);
    assert_eq!(final_file["ai_strong"], 10);
    let sidecar = fixture.sidecar_value(&commit);
    let replacement = attribution_at_line(&sidecar, "final.py", 15);
    assert_eq!(replacement["category"], "unknown");
    assert_eq!(replacement["primaryBasis"], Value::Null);
    assert_eq!(replacement["score"], Value::Null);
    fixture.drop_root();
}

#[test]
fn time_cut_new_file_pure_addition_stays_unknown_and_snapshot_lines_stay_strong() {
    let fixture = Fixture::new("time-cut-new-file-pure-addition");
    let final_contents = format!("{PALINDROME_AI_SNAPSHOT}    print(\"new user line\")\n");
    let commit = time_cut_new_file(&fixture, &final_contents, "user adds a line");
    let report = fixture.derived_value(&commit);
    let final_file = file(&report, "final.py");
    assert_eq!(final_file["ai_strong"], 11);
    assert_eq!(final_file["mixed"], 0);
    assert_eq!(final_file["unknown"], 1);
    let sidecar = fixture.sidecar_value(&commit);
    assert_eq!(
        attribution_at_line(&sidecar, "final.py", 16)["category"],
        "unknown"
    );
    assert_eq!(
        attribution_at_line(&sidecar, "final.py", 15)["category"],
        "ai_strong"
    );
    fixture.drop_root();
}

#[test]
fn exclusion_classifier_metadata_and_counts_are_versioned_and_count_neutral() {
    let fixture = Fixture::new("exclusion-classifier-mixed-commit");
    fixture.hook("PreToolUse", "exclusion-metadata");
    fs::create_dir_all(fixture.repo.join("src")).unwrap();
    fs::create_dir_all(fixture.repo.join("node_modules/pkg")).unwrap();
    fs::create_dir_all(fixture.repo.join("vendor/lib")).unwrap();
    fs::write(
        fixture.repo.join("src/main.rs"),
        "fn main() {\n    println!(\"ordinary source\");\n}\n",
    )
    .unwrap();
    fs::write(
        fixture.repo.join("Cargo.lock"),
        "version = 3\n[[package]]\n",
    )
    .unwrap();
    fs::write(
        fixture.repo.join("generated.rs"),
        "// Code generated by fixture. DO NOT EDIT.\npub fn generated() {}\n",
    )
    .unwrap();
    fs::write(
        fixture.repo.join("node_modules/pkg/index.js"),
        "module.exports = 1;\n",
    )
    .unwrap();
    fs::write(
        fixture.repo.join("vendor/lib/lib.rs"),
        "pub fn vendored() {}\n",
    )
    .unwrap();
    fs::write(fixture.repo.join("image.png"), b"PNG bytes\0not source\n").unwrap();
    fixture.hook("PostToolUse", "exclusion-metadata");

    let commit = fixture.commit("mixed included and excluded additions");
    assert!(derive(&fixture, &commit).status.success());
    let report = fixture.derived_value(&commit);
    assert_eq!(report["totals"]["e"], 3);
    assert_eq!(report["totals"]["ai_direct"], 0);
    assert_eq!(report["totals"]["ai_strong"], 3);
    assert_eq!(report["totals"]["ai_probable"], 0);
    assert_eq!(report["totals"]["mixed"], 0);
    assert_eq!(report["totals"]["unknown"], 0);
    assert_eq!(report["totals"]["excluded"], 7);
    assert_eq!(report["totals"]["ai_deterministic_pct"], 100.0);
    assert_eq!(report["totals"]["ai_linked_pct"], 100.0);
    assert_eq!(file(&report, "src/main.rs")["ai_strong"], 3);

    let sidecar = fixture.sidecar_value(&commit);
    let attributions = sidecar["attributions"].as_array().unwrap();
    let excluded_ranges = attributions
        .iter()
        .filter(|range| range["category"] == "excluded")
        .collect::<Vec<_>>();
    assert_eq!(excluded_ranges.len(), 5);
    for (path, reason) in [
        ("Cargo.lock", "LOCKFILE"),
        ("generated.rs", "GENERATED_HEADER"),
        ("node_modules/pkg/index.js", "DEPENDENCY_DIRECTORY"),
        ("vendor/lib/lib.rs", "VENDORED_DIRECTORY"),
        ("image.png", "BINARY_CONTENT"),
    ] {
        let range = excluded_ranges
            .iter()
            .find(|range| range["path"] == path)
            .unwrap_or_else(|| panic!("missing excluded range for {path}"));
        assert_eq!(range["exclusionClassifier"], "exclusion-v1");
        assert_eq!(range["exclusionReason"], reason);
        assert!(range["category"] == "excluded");
        assert!(range["score"].is_null());
        assert!(range["primaryBasis"].is_null());
        let path_ranges = attributions
            .iter()
            .filter(|candidate| candidate["path"] == path)
            .collect::<Vec<_>>();
        assert!(!path_ranges.is_empty());
        assert!(path_ranges
            .iter()
            .all(|candidate| candidate["category"] == "excluded"));
    }
    let ordinary = excluded_ranges
        .iter()
        .find(|range| range["path"] == "src/main.rs");
    assert!(ordinary.is_none());

    let summary_output = fixture.cli(&[
        "summary",
        "--repo",
        fixture.repo.to_str().unwrap(),
        "--commit",
        &commit,
        "--file",
        "Cargo.lock",
        "--format",
        "json",
    ]);
    assert!(summary_output.status.success());
    let summary: Value = serde_json::from_slice(&summary_output.stdout).unwrap();
    assert_eq!(summary["versions"]["exclusion_classifier"], "exclusion-v1");
    assert_eq!(summary["totals"]["eligible"], 3);
    assert_eq!(summary["totals"]["excluded"], 7);
    let range = &summary["file_ranges"][0];
    assert_eq!(range["category"], "excluded");
    assert_eq!(range["exclusionClassifier"], "exclusion-v1");
    assert_eq!(range["exclusionReason"], "LOCKFILE");

    let table = fixture.cli(&[
        "summary",
        "--repo",
        fixture.repo.to_str().unwrap(),
        "--commit",
        &commit,
        "--file",
        "Cargo.lock",
        "--format",
        "table",
    ]);
    assert!(table.status.success());
    let table = String::from_utf8_lossy(&table.stdout);
    assert!(table.contains("exclusionClassifier\texclusionReason"));
    assert!(table.contains("exclusion-v1\tLOCKFILE"));
    fixture.drop_root();
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
fn codex_apply_patch_snapshot_ranges_receive_direct_labels() {
    let new_file = Fixture::new("direct-new-file");
    new_file.hook("PreToolUse", "patch-new-file");
    fs::write(new_file.repo.join("d.py"), b"first line\nsecond line\n").unwrap();
    new_file.hook("PostToolUse", "patch-new-file");
    configure_structured_apply_patch(&new_file, "d.py", Some(true), false, json!([[1, 2]]));
    let commit = new_file.commit("add structured file");
    assert!(derive(&new_file, &commit).status.success());
    let report = new_file.derived_value(&commit);
    assert_eq!(file(&report, "d.py")["ai_direct"], 2);
    assert_eq!(file(&report, "d.py")["ai_strong"], 0);
    let sidecar = sidecar_value(&new_file, &commit);
    let direct = attribution_for(&sidecar, "d.py", "ai_direct");
    assert_eq!(direct["startLine"], 1);
    assert_eq!(direct["endLine"], 2);
    assert_eq!(direct["score"], 0.99);
    assert_eq!(direct["primaryBasis"], "DIRECT_AGENT_PATCH");
    assert_eq!(direct["limitingBasis"], json!([]));
    new_file.drop_root();

    let existing = Fixture::new("direct-existing-file");
    existing.hook("PreToolUse", "patch-existing-file");
    fs::write(
        existing.repo.join("tracked.txt"),
        b"one\nchanged by patch\n",
    )
    .unwrap();
    existing.hook("PostToolUse", "patch-existing-file");
    configure_structured_apply_patch(&existing, "tracked.txt", Some(true), false, json!([[2, 2]]));
    let commit = existing.commit("edit structured file");
    assert!(derive(&existing, &commit).status.success());
    let report = existing.derived_value(&commit);
    assert_eq!(file(&report, "tracked.txt")["ai_direct"], 1);
    assert_eq!(file(&report, "tracked.txt")["ai_strong"], 0);
    let sidecar = sidecar_value(&existing, &commit);
    let lines = sidecar["files"][0]["lines"].as_array().unwrap();
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0]["line"], 2);
    assert_eq!(lines[0]["label"], "d");
    existing.drop_root();
}

#[test]
fn detected_renames_count_only_added_lines_and_ignore_git_rename_config() {
    let fixture = Fixture::new("rename-eligibility");
    fixture.hook("PreToolUse", "rename-source-ai");
    fs::write(fixture.repo.join("rename-source.txt"), b"one\ntwo\nthree\n").unwrap();
    fixture.hook("PostToolUse", "rename-source-ai");
    configure_structured_apply_patch(
        &fixture,
        "rename-source.txt",
        Some(true),
        false,
        json!([[1, 3]]),
    );
    let source = fixture.commit("add rename source");
    assert!(derive(&fixture, &source).status.success());

    git(&fixture.repo, &["config", "diff.renames", "false"]);
    git(
        &fixture.repo,
        &["mv", "rename-source.txt", "rename-destination.txt"],
    );
    let pure_rename = fixture.commit("pure rename");
    assert!(derive(&fixture, &pure_rename).status.success());
    let pure = fixture.derived_value(&pure_rename);
    assert_eq!(pure["totals"]["e"], 0);
    assert_eq!(pure["files"].as_array().unwrap().len(), 1);
    assert_eq!(pure["files"][0]["path"], "rename-destination.txt");
    let pure_sidecar = fixture.sidecar_value(&pure_rename);
    let carried: Vec<_> = pure_sidecar["attributions"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|range| range["path"] == "rename-destination.txt")
        .collect();
    assert_eq!(carried.len(), 3);
    assert!(carried.iter().all(|range| range["category"] == "ai_direct"));
    assert!(carried
        .iter()
        .all(|range| range["primaryBasis"] == "MOVE_LINEAGE"));
    let summary = fixture.cli(&[
        "summary",
        "--repo",
        fixture.repo.to_str().unwrap(),
        "--commit",
        &pure_rename,
        "--file",
        "rename-destination.txt",
        "--format",
        "json",
    ]);
    assert!(summary.status.success());
    let summary: Value = serde_json::from_slice(&summary.stdout).unwrap();
    assert_eq!(summary["commits"][0]["eligible"], 0);
    assert!(summary["commits"][0]["deterministic_pct"].is_null());
    assert!(summary["commits"][0]["linked_pct"].is_null());
    assert_eq!(summary["commits"][0]["files"], 1);
    assert_eq!(summary["file_ranges"].as_array().unwrap().len(), 3);

    fs::write(
        fixture.repo.join("rename-destination.txt"),
        b"one\ntwo\nthree\nfour\nfive\n",
    )
    .unwrap();
    let edited_rename = fixture.commit("rename with two added lines");
    assert!(derive(&fixture, &edited_rename).status.success());
    let edited = fixture.derived_value(&edited_rename);
    assert_eq!(edited["totals"]["e"], 2);

    for index in 0..27 {
        let old = format!("mass-source-{index}.txt");
        fs::write(fixture.repo.join(&old), b"unchanged content\n").unwrap();
    }
    fixture.commit("add mass rename sources");
    for index in 0..27 {
        let old = format!("mass-source-{index}.txt");
        let new = format!("mass-destination-{index}.txt");
        git(&fixture.repo, &["mv", &old, &new]);
    }
    let mass_rename = fixture.commit("mass rename");
    assert!(derive(&fixture, &mass_rename).status.success());
    let mass = fixture.derived_value(&mass_rename);
    assert_eq!(mass["totals"]["e"], 0);
    assert_eq!(mass["files"].as_array().unwrap().len(), 27);
    fixture.drop_root();
}

#[test]
fn delayed_commit_uses_only_unconsumed_exact_ai_snapshot() {
    const FIXED_DATE: &str = "@1712318400 +0000";
    const SAME_SECOND_EVENT_TIME: u128 = 1_712_318_400_000_000_001;

    let delayed = Fixture::new("delayed-commit-exact");
    git_with_dates(
        &delayed.repo,
        &["commit", "--amend", "--no-edit"],
        FIXED_DATE,
    );
    delayed.hook("PreToolUse", "delayed-file");
    fs::write(
        delayed.repo.join("delayed.txt"),
        b"AI line one\nAI line two\n",
    )
    .unwrap();
    delayed.hook("PostToolUse", "delayed-file");
    set_ai_event_timestamp(&delayed, "delayed.txt", SAME_SECOND_EVENT_TIME);
    fs::write(delayed.repo.join("unrelated.txt"), b"unrelated\n").unwrap();
    git(&delayed.repo, &["add", "unrelated.txt"]);
    git_with_dates(
        &delayed.repo,
        &["commit", "-m", "unrelated change"],
        FIXED_DATE,
    );
    let intervening = git_text(&delayed.repo, &["rev-parse", "HEAD"]);
    assert!(derive(&delayed, &intervening).status.success());
    fs::write(delayed.repo.join("current.txt"), b"current human file\n").unwrap();
    let committed = delayed.commit_at("commit delayed AI file", FIXED_DATE);
    assert!(derive(&delayed, &committed).status.success());
    let report = delayed.derived_value(&committed);
    assert_eq!(file(&report, "delayed.txt")["ai_strong"], 2);
    assert_eq!(file(&report, "current.txt")["unknown"], 1);
    let sidecar = delayed.sidecar_value(&committed);
    let ranges = sidecar["attributions"].as_array().unwrap();
    let delayed_ranges = ranges
        .iter()
        .filter(|range| range["path"] == "delayed.txt")
        .collect::<Vec<_>>();
    assert!(!delayed_ranges.is_empty());
    assert!(delayed_ranges.iter().all(|range| {
        range["primaryBasis"] == "EXACT_FRAGMENT_MATCH"
            && range["limitingBasis"]
                .as_array()
                .unwrap()
                .contains(&json!("X_DELAYED_COMMIT"))
    }));
    delayed.drop_root();

    let edited = Fixture::new("delayed-commit-human-edit");
    git_with_dates(
        &edited.repo,
        &["commit", "--amend", "--no-edit"],
        FIXED_DATE,
    );
    edited.hook("PreToolUse", "delayed-edited-file");
    fs::write(edited.repo.join("delayed.txt"), b"AI original\n").unwrap();
    edited.hook("PostToolUse", "delayed-edited-file");
    set_ai_event_timestamp(&edited, "delayed.txt", SAME_SECOND_EVENT_TIME);
    fs::write(edited.repo.join("unrelated.txt"), b"unrelated\n").unwrap();
    git(&edited.repo, &["add", "unrelated.txt"]);
    git_with_dates(
        &edited.repo,
        &["commit", "-m", "unrelated change"],
        FIXED_DATE,
    );
    let intervening = git_text(&edited.repo, &["rev-parse", "HEAD"]);
    assert!(derive(&edited, &intervening).status.success());
    fs::write(edited.repo.join("delayed.txt"), b"human replacement\n").unwrap();
    let committed = edited.commit_at("commit hand edited file", FIXED_DATE);
    assert!(derive(&edited, &committed).status.success());
    let edited_report = edited.derived_value(&committed);
    let edited_file = file(&edited_report, "delayed.txt");
    assert_eq!(edited_file["evidence"], "time_cut");
    assert_eq!(edited_file["unknown"], 1);
    assert_eq!(edited_file["ai_strong"], 0);
    edited.drop_root();

    let consumed = Fixture::new("delayed-commit-consumed");
    git_with_dates(
        &consumed.repo,
        &["commit", "--amend", "--no-edit"],
        FIXED_DATE,
    );
    consumed.hook("PreToolUse", "consumed-file");
    fs::write(consumed.repo.join("consumed.txt"), b"AI original\n").unwrap();
    consumed.hook("PostToolUse", "consumed-file");
    set_ai_event_timestamp(&consumed, "consumed.txt", SAME_SECOND_EVENT_TIME);
    let first = consumed.commit_at("consume AI event", FIXED_DATE);
    assert!(derive(&consumed, &first).status.success());
    fs::remove_file(consumed.repo.join("consumed.txt")).unwrap();
    let deleted = consumed.commit_at("delete previously committed file", FIXED_DATE);
    assert!(derive(&consumed, &deleted).status.success());
    fs::write(consumed.repo.join("consumed.txt"), b"AI original\n").unwrap();
    let readded = consumed.commit_at("re-add without new evidence", FIXED_DATE);
    assert!(derive(&consumed, &readded).status.success());
    assert_eq!(
        file(&consumed.derived_value(&readded), "consumed.txt")["unknown"],
        1
    );
    consumed.drop_root();
}

#[test]
fn real_codex_apply_patch_existing_file_appends_direct_lines() {
    const PATH: &str = "src/vs/base/common/arrays.ts";
    let before = include_bytes!("fixtures/real_apply_patch_existing_file/before.bin");
    let after = include_bytes!("fixtures/real_apply_patch_existing_file/after.bin");
    let source_event: Value = serde_json::from_str(include_str!(
        "fixtures/real_apply_patch_existing_file/event.jsonl"
    ))
    .unwrap();
    assert_eq!(
        source_event["before_sha"],
        format!("{:x}", Sha256::digest(before))
    );
    assert_eq!(
        source_event["after_sha"],
        format!("{:x}", Sha256::digest(after))
    );

    let (fixture, commit, report) = structured_existing_file_commit(
        "real-direct-existing-append",
        PATH,
        before,
        after,
        json!([[950, 951]]),
    );
    assert_eq!(file(&report, PATH)["ai_direct"], 2);
    assert_eq!(file(&report, PATH)["ai_strong"], 0);
    let sidecar = fixture.sidecar_value(&commit);
    let direct = attribution_for(&sidecar, PATH, "ai_direct");
    assert_eq!(direct["startLine"], 950);
    assert_eq!(direct["endLine"], 951);
    assert_eq!(direct["score"], 0.99);
    assert_eq!(direct["primaryBasis"], "DIRECT_AGENT_PATCH");
    fixture.drop_root();
}

#[test]
fn structured_apply_patch_hashes_support_lf_crlf_and_mixed_line_endings() {
    let cases = [
        (
            "crlf-before-after",
            b"one\r\nbefore\r\n".as_slice(),
            b"one\r\nafter\r\n".as_slice(),
            "true",
        ),
        (
            "lf-before-after",
            b"one\nbefore\n".as_slice(),
            b"one\nafter\n".as_slice(),
            "false",
        ),
        (
            "mixed-before-after",
            b"one\r\nbefore\r\n".as_slice(),
            b"one\nafter\n".as_slice(),
            "true",
        ),
    ];
    for (name, before, after, autocrlf) in cases {
        let (fixture, _commit, report) = structured_existing_file_commit_with_options(
            name,
            "src/eol.txt",
            before,
            after,
            json!([[2, 2]]),
            StructuredFixtureOptions {
                autocrlf: Some(autocrlf),
                ..StructuredFixtureOptions::default()
            },
        );
        assert_eq!(file(&report, "src/eol.txt")["ai_direct"], 1, "{name}");
        assert_eq!(file(&report, "src/eol.txt")["ai_strong"], 0, "{name}");
        fixture.drop_root();
    }

    let (mismatch, commit, report) = structured_existing_file_commit_with_options(
        "crlf-before-hash-mismatch",
        "src/eol.txt",
        b"one\r\nbefore\r\n",
        b"one\r\nafter\r\n",
        json!([[2, 2]]),
        StructuredFixtureOptions {
            autocrlf: Some("true"),
            corrupt_hash: Some("before_sha"),
            ..StructuredFixtureOptions::default()
        },
    );
    assert_eq!(file(&report, "src/eol.txt")["ai_direct"], 0);
    assert_eq!(file(&report, "src/eol.txt")["ai_strong"], 1);
    assert_eq!(mismatch.sidecar_value(&commit)["totals"]["ai_probable"], 0);
    mismatch.drop_root();

    let (missing, _commit, report) = structured_existing_file_commit_with_options(
        "crlf-missing-after-blob",
        "src/eol.txt",
        b"one\r\nbefore\r\n",
        b"one\r\nafter\r\n",
        json!([[2, 2]]),
        StructuredFixtureOptions {
            autocrlf: Some("false"),
            remove_after_blob: true,
            ..StructuredFixtureOptions::default()
        },
    );
    assert_eq!(file(&report, "src/eol.txt")["ai_direct"], 0);
    assert_eq!(file(&report, "src/eol.txt")["unknown"], 1);
    assert_eq!(file(&report, "src/eol.txt")["ai_probable"], 0);
    missing.drop_root();
}

#[test]
fn codex_apply_patch_middle_insert_in_existing_file_is_direct() {
    let (fixture, commit, report) = structured_existing_file_commit(
        "direct-middle-insert",
        "src/middle.txt",
        b"alpha\nbeta\ngamma\n",
        b"alpha\ninserted in the middle\nbeta\ngamma\n",
        json!([[2, 2]]),
    );
    assert_eq!(file(&report, "src/middle.txt")["ai_direct"], 1);
    assert_eq!(file(&report, "src/middle.txt")["ai_strong"], 0);
    let sidecar = fixture.sidecar_value(&commit);
    let direct = attribution_for(&sidecar, "src/middle.txt", "ai_direct");
    assert_eq!(direct["startLine"], 2);
    assert_eq!(direct["primaryBasis"], "DIRECT_AGENT_PATCH");
    fixture.drop_root();
}

#[test]
fn codex_apply_patch_replacing_existing_line_is_direct() {
    let (fixture, commit, report) = structured_existing_file_commit(
        "direct-line-replacement",
        "src/replaced.txt",
        b"alpha\nbeta\n",
        b"alpha\nreplaced beta line\n",
        json!([[2, 2]]),
    );
    assert_eq!(file(&report, "src/replaced.txt")["ai_direct"], 1);
    assert_eq!(file(&report, "src/replaced.txt")["ai_strong"], 0);
    let sidecar = fixture.sidecar_value(&commit);
    let direct = attribution_for(&sidecar, "src/replaced.txt", "ai_direct");
    assert_eq!(direct["startLine"], 2);
    assert_eq!(direct["primaryBasis"], "DIRECT_AGENT_PATCH");
    fixture.drop_root();
}

#[test]
fn explain_reports_direct_shell_mixed_unknown_and_excluded_decisions_read_only() {
    let (direct, commit, _) = structured_existing_file_commit(
        "explain-direct",
        "src/direct.txt",
        b"before\n",
        b"direct line\n",
        json!([[1, 1]]),
    );
    let before_store = snapshot_store(&direct);
    let direct_result = explain_json(&direct, &commit, "src/direct.txt", Some(1));
    assert_eq!(direct_result["lines"][0]["category"], "ai_direct");
    assert_eq!(direct_result["lines"][0]["score"], 0.99);
    assert_eq!(
        direct_result["lines"][0]["firstFailedCondition"],
        Value::Null
    );
    assert!(direct_result["lines"][0]["checks"]
        .as_array()
        .unwrap()
        .iter()
        .any(|check| check["condition"] == "before_sha" && check["passed"] == true));
    let table = direct.cli(&[
        "explain",
        "--repo",
        direct.repo.to_str().unwrap(),
        "--commit",
        &commit,
        "--file",
        "src/direct.txt",
        "--line",
        "1",
    ]);
    assert!(table.status.success());
    assert!(String::from_utf8_lossy(&table.stdout).contains("ai_direct"));
    assert_eq!(snapshot_store(&direct), before_store);
    direct.drop_root();

    let (shell, shell_commit, _) = structured_existing_file_commit(
        "explain-shell",
        "src/shell.txt",
        b"before\n",
        b"shell line\n",
        json!([[1, 1]]),
    );
    set_fixture_apply_patch_field(&shell, "src/shell.txt", "operation", json!("Bash"));
    set_fixture_apply_patch_field(&shell, "src/shell.txt", "structured_edit", json!(false));
    assert!(derive(&shell, &shell_commit).status.success());
    let shell_result = explain_json(&shell, &shell_commit, "src/shell.txt", None);
    assert_eq!(shell_result["lines"][0]["category"], "ai_strong");
    assert_eq!(
        shell_result["lines"][0]["firstFailedCondition"],
        "structured_edit"
    );
    shell.drop_root();

    let mixed = Fixture::new("explain-mixed");
    fs::write(mixed.repo.join("tracked.txt"), b"before\noriginal\n").unwrap();
    let _parent = mixed.commit("mixed parent");
    mixed.hook("PreToolUse", "explain-ai");
    fs::write(mixed.repo.join("tracked.txt"), b"before\nai edit line\n").unwrap();
    mixed.hook("PostToolUse", "explain-ai");
    configure_structured_apply_patch(&mixed, "tracked.txt", Some(true), false, json!([[2, 2]]));
    mixed.hook("PreToolUse", "explain-human");
    fs::write(mixed.repo.join("tracked.txt"), b"before\nhuman edit line\n").unwrap();
    mixed.hook("PostToolUse", "explain-human");
    mark_event_external(&mixed, "explain-human");
    let mixed_commit = mixed.commit("mixed final");
    assert!(derive(&mixed, &mixed_commit).status.success());
    let mixed_result = explain_json(&mixed, &mixed_commit, "tracked.txt", None);
    assert_eq!(mixed_result["lines"][0]["category"], "mixed");
    mixed.drop_root();

    let unknown = Fixture::new("explain-unknown");
    fs::write(unknown.repo.join("unknown.txt"), b"unobserved line\n").unwrap();
    let unknown_commit = unknown.commit("unknown addition");
    assert!(derive(&unknown, &unknown_commit).status.success());
    let unknown_result = explain_json(&unknown, &unknown_commit, "unknown.txt", None);
    assert_eq!(unknown_result["lines"][0]["category"], "unknown");
    assert_eq!(
        unknown_result["lines"][0]["firstFailedCondition"],
        "final_attribution"
    );
    assert!(unknown_result["lines"][0]["checks"]
        .as_array()
        .unwrap()
        .iter()
        .any(|check| {
            check["condition"] == "final_attribution"
                && check["compared"]
                    .as_str()
                    .unwrap()
                    .contains("committed_alignment")
        }));
    unknown.drop_root();

    let excluded = Fixture::new("explain-excluded");
    fs::write(
        excluded.repo.join("package-lock.json"),
        b"{\"lockfileVersion\":3}\n",
    )
    .unwrap();
    let excluded_commit = excluded.commit("excluded lockfile");
    assert!(derive(&excluded, &excluded_commit).status.success());
    let excluded_result = explain_json(&excluded, &excluded_commit, "package-lock.json", None);
    assert_eq!(excluded_result["lines"][0]["category"], "excluded");
    assert_eq!(
        excluded_result["lines"][0]["firstFailedCondition"],
        "exclusion_rules"
    );
    excluded.drop_root();
}

#[test]
fn explain_blank_line_prints_eligibility_message() {
    let fixture = Fixture::new("explain-blank-line");
    fixture.hook("PreToolUse", "blank-line-tool");
    fs::write(fixture.repo.join("tracked.txt"), b"one\ntwo\n\n").unwrap();
    fixture.hook("PostToolUse", "blank-line-tool");
    let commit = fixture.commit("add blank line");
    assert!(derive(&fixture, &commit).status.success());
    let result = explain_json(&fixture, &commit, "tracked.txt", Some(3));
    assert!(result["lines"].as_array().unwrap().is_empty());
    let table = prov_capture::commit::explain_table(&result).unwrap();
    assert!(table.contains("blank line, not eligible"));
    fixture.drop_root();
}

#[test]
fn structured_direct_evidence_respects_human_edits_and_limits() {
    let human_edit = Fixture::new("direct-human-edit");
    human_edit.hook("PreToolUse", "patch-human-edit");
    fs::write(human_edit.repo.join("d.py"), b"first line\nsecond line\n").unwrap();
    human_edit.hook("PostToolUse", "patch-human-edit");
    configure_structured_apply_patch(&human_edit, "d.py", Some(true), false, json!([[1, 2]]));
    human_edit.hook("PreToolUse", "human-followup");
    fs::write(
        human_edit.repo.join("d.py"),
        b"human replacement\nsecond line\n",
    )
    .unwrap();
    human_edit.hook("PostToolUse", "human-followup");
    mark_event_external(&human_edit, "human-followup");
    let commit = human_edit.commit("human edits one patch line");
    assert!(derive(&human_edit, &commit).status.success());
    let sidecar = sidecar_value(&human_edit, &commit);
    assert_eq!(attribution_for(&sidecar, "d.py", "mixed")["startLine"], 1);
    assert_eq!(
        attribution_for(&sidecar, "d.py", "ai_direct")["startLine"],
        2
    );
    human_edit.drop_root();

    let bash = Fixture::new("direct-bash-fallback");
    bash.hook("PreToolUse", "bash-write");
    fs::write(bash.repo.join("tracked.txt"), b"one\nbash line\n").unwrap();
    bash.hook("PostToolUse", "bash-write");
    configure_structured_apply_patch(&bash, "tracked.txt", Some(false), false, json!([[2, 2]]));
    let commit = bash.commit("shell write fallback");
    assert!(derive(&bash, &commit).status.success());
    assert_eq!(
        file(&bash.derived_value(&commit), "tracked.txt")["ai_strong"],
        1
    );
    bash.drop_root();

    let mismatch = Fixture::new("direct-range-mismatch");
    mismatch.hook("PreToolUse", "mismatched-range");
    fs::write(mismatch.repo.join("tracked.txt"), b"one\nchanged\n").unwrap();
    mismatch.hook("PostToolUse", "mismatched-range");
    configure_structured_apply_patch(&mismatch, "tracked.txt", Some(true), false, json!([[1, 1]]));
    let commit = mismatch.commit("mismatched structured range");
    assert!(derive(&mismatch, &commit).status.success());
    assert_eq!(
        file(&mismatch.derived_value(&commit), "tracked.txt")["ai_strong"],
        1
    );
    mismatch.drop_root();

    let incomplete = Fixture::new("direct-incomplete-evidence");
    incomplete.hook("PreToolUse", "incomplete-patch");
    fs::write(
        incomplete.repo.join("tracked.txt"),
        b"one\nincomplete evidence\n",
    )
    .unwrap();
    incomplete.hook("PostToolUse", "incomplete-patch");
    configure_structured_apply_patch(
        &incomplete,
        "tracked.txt",
        Some(true),
        false,
        json!([[2, 2]]),
    );
    set_ai_change_field(&incomplete, "after_incomplete", json!(true));
    let commit = incomplete.commit("incomplete structured evidence");
    assert!(derive(&incomplete, &commit).status.success());
    let sidecar = sidecar_value(&incomplete, &commit);
    assert_eq!(sidecar["totals"]["ai_direct"], 0);
    let strong = attribution_for(&sidecar, "tracked.txt", "ai_strong");
    assert!(strong["limitingBasis"]
        .as_array()
        .unwrap()
        .contains(&json!("X_DEGRADED")));
    incomplete.drop_root();

    let overlap = Fixture::new("direct-overlap-cap");
    overlap.hook("PreToolUse", "overlapping-patch");
    fs::write(overlap.repo.join("tracked.txt"), b"one\noverlap line\n").unwrap();
    overlap.hook("PostToolUse", "overlapping-patch");
    configure_structured_apply_patch(&overlap, "tracked.txt", Some(true), true, json!([[2, 2]]));
    set_ai_change_field(&overlap, "overlap", json!(true));
    let commit = overlap.commit("overlap limits direct confidence");
    assert!(derive(&overlap, &commit).status.success());
    let sidecar = sidecar_value(&overlap, &commit);
    let probable = attribution_for(&sidecar, "tracked.txt", "ai_probable");
    assert!(probable["limitingBasis"]
        .as_array()
        .unwrap()
        .contains(&json!("X_OVERLAPPING_SESSIONS")));
    assert_eq!(sidecar["totals"]["ai_direct"], 0);
    overlap.drop_root();

    let legacy = Fixture::new("direct-legacy-event");
    legacy.hook("PreToolUse", "legacy-event");
    fs::write(legacy.repo.join("tracked.txt"), b"one\nlegacy line\n").unwrap();
    legacy.hook("PostToolUse", "legacy-event");
    configure_structured_apply_patch(&legacy, "tracked.txt", None, false, json!([[2, 2]]));
    let commit = legacy.commit("legacy event without structured flag");
    assert!(derive(&legacy, &commit).status.success());
    assert_eq!(
        file(&legacy.derived_value(&commit), "tracked.txt")["ai_strong"],
        1
    );
    legacy.drop_root();

    let duplicate = Fixture::new("direct-duplicate-lines");
    duplicate.hook("PreToolUse", "duplicate-patch");
    fs::write(duplicate.repo.join("d.py"), b"repeat call\n").unwrap();
    duplicate.hook("PostToolUse", "duplicate-patch");
    configure_structured_apply_patch(&duplicate, "d.py", Some(true), false, json!([[1, 1]]));
    duplicate.hook("PreToolUse", "duplicate-copy");
    fs::write(duplicate.repo.join("d.py"), b"repeat call\nrepeat call\n").unwrap();
    duplicate.hook("PostToolUse", "duplicate-copy");
    mark_event_external(&duplicate, "duplicate-copy");
    let commit = duplicate.commit("ambiguous duplicate patch lines");
    assert!(derive(&duplicate, &commit).status.success());
    let duplicate_sidecar = sidecar_value(&duplicate, &commit);
    assert_eq!(duplicate_sidecar["totals"]["ai_direct"], 0);
    assert!(duplicate_sidecar["files"][0]["lines"]
        .as_array()
        .unwrap()
        .iter()
        .all(|line| line["label"] != "d"));
    duplicate.drop_root();
}

#[test]
fn sidecar_metadata_is_additive_coalesced_and_count_neutral() {
    let fixture = Fixture::new("line-metadata");
    fixture.hook("PreToolUse", "metadata-tool");
    fs::write(
        fixture.repo.join("tracked.txt"),
        b"one\ntwo\nai first\nai second\n",
    )
    .unwrap();
    fixture.hook("PostToolUse", "metadata-tool");
    let commit = fixture.commit("metadata lines");
    assert!(derive(&fixture, &commit).status.success());

    let report: Value =
        serde_json::from_slice(&fs::read(fixture.derived_path(&commit)).unwrap()).unwrap();
    let sidecar: Value =
        serde_json::from_slice(&fs::read(fixture.lines_path(&commit)).unwrap()).unwrap();
    assert_eq!(sidecar["schema"], "prov.lines/0.1");
    assert_eq!(sidecar["totals"]["e"], report["totals"]["e"]);
    assert_eq!(
        sidecar["totals"]["ai_strong"],
        report["totals"]["ai_strong"]
    );
    assert_eq!(
        sidecar["totals"]["ai_probable"],
        report["totals"]["ai_probable"]
    );
    assert_eq!(sidecar["totals"]["mixed"], report["totals"]["mixed"]);
    assert_eq!(sidecar["totals"]["unknown"], report["totals"]["unknown"]);
    assert_eq!(report["totals"]["ai_direct"], 0);
    let ranges = sidecar["attributions"].as_array().unwrap();
    let tracked: Vec<_> = ranges
        .iter()
        .filter(|range| range["path"] == "tracked.txt")
        .collect();
    assert_eq!(tracked.len(), 1);
    assert_eq!(tracked[0]["startLine"], 3);
    assert_eq!(tracked[0]["endLine"], 4);
    assert_eq!(tracked[0]["endLineExclusive"], 5);
    assert_eq!(tracked[0]["category"], "ai_strong");
    assert_eq!(tracked[0]["score"], 0.92);
    assert_eq!(tracked[0]["confidenceBand"], "high");
    assert_eq!(tracked[0]["primaryBasis"], "EXACT_FRAGMENT_MATCH");
    assert_eq!(tracked[0]["source"]["adapterId"], "codex");
    assert_eq!(tracked[0]["source"]["toolId"], "codex");
    assert_eq!(tracked[0]["source"]["sessionRef"], "metadata-tool");
    assert_eq!(tracked[0]["operation"], "PowerShell");
    assert_eq!(tracked[0]["algorithmProfile"], "deterministic-v1");
    assert_eq!(tracked[0]["scoreProfile"], "score-v1");
    assert_eq!(tracked[0]["limitingBasis"], json!([]));
    assert_eq!(tracked[0]["supportingBasis"], json!([]));
    assert_eq!(tracked[0]["source"]["toolId"], "codex");
    assert_eq!(tracked[0]["source"]["sessionRef"], "metadata-tool");
    assert_eq!(report["totals"]["e"], 2);
    assert_eq!(report["totals"]["ai_strong"], 2);
    assert_eq!(report["totals"]["ai_probable"], 0);
    assert_eq!(report["totals"]["mixed"], 0);
    assert_eq!(report["totals"]["unknown"], 0);
    assert_eq!(report["totals"]["ai_deterministic_pct"], 100.0);
    assert_eq!(report["totals"]["ai_linked_pct"], 100.0);
    assert_eq!(sidecar["totals"]["e"], 2);
    assert_eq!(sidecar["totals"]["ai_strong"], 2);
    assert_eq!(sidecar["totals"]["ai_probable"], 0);
    assert_eq!(sidecar["totals"]["mixed"], 0);
    assert_eq!(sidecar["totals"]["unknown"], 0);
    fixture.drop_root();
}

#[test]
fn score_metadata_covers_evidence_limitations_and_no_score_categories() {
    let time_cut = Fixture::new("metadata-time-cut");
    time_cut.hook("PreToolUse", "time-cut-tool");
    fs::write(time_cut.repo.join("tracked.txt"), b"one\nai line\n").unwrap();
    time_cut.hook("PostToolUse", "time-cut-tool");
    fs::write(time_cut.repo.join("tracked.txt"), b"one\nai line\nlate\n").unwrap();
    let time_cut_commit = time_cut.commit("time cut");
    assert!(derive(&time_cut, &time_cut_commit).status.success());
    let time_cut_sidecar = sidecar_value(&time_cut, &time_cut_commit);
    let time_cut_range = attribution_for(&time_cut_sidecar, "tracked.txt", "ai_strong");
    assert_eq!(time_cut_range["score"], 0.92);
    assert_eq!(time_cut_range["confidenceBand"], "high");
    assert!(time_cut_range["limitingBasis"]
        .as_array()
        .unwrap()
        .contains(&json!("X_UNOBSERVED_EDIT_IN_FILE")));
    time_cut.drop_root();

    for (name, field, expected_basis, expected_category, expected_score) in [
        (
            "metadata-overlap",
            "overlap",
            "X_OVERLAPPING_SESSIONS",
            "ai_probable",
            0.79,
        ),
        (
            "metadata-degraded",
            "degraded",
            "X_DEGRADED",
            "ai_probable",
            0.79,
        ),
        (
            "metadata-incomplete",
            "after_incomplete",
            "X_DEGRADED",
            "ai_strong",
            0.82,
        ),
    ] {
        let fixture = Fixture::new(name);
        fixture.hook("PreToolUse", "limited-tool");
        fs::write(fixture.repo.join("tracked.txt"), b"one\nai line\n").unwrap();
        fixture.hook("PostToolUse", "limited-tool");
        set_ai_change_field(&fixture, field, json!(true));
        let commit = fixture.commit("limited evidence");
        assert!(derive(&fixture, &commit).status.success());
        let sidecar = sidecar_value(&fixture, &commit);
        let range = attribution_for(&sidecar, "tracked.txt", expected_category);
        assert_eq!(range["score"], expected_score);
        assert_eq!(
            range["confidenceBand"],
            if expected_score >= 0.8 {
                "high"
            } else {
                "medium"
            }
        );
        assert!(range["limitingBasis"]
            .as_array()
            .unwrap()
            .contains(&json!(expected_basis)));
        fixture.drop_root();
    }

    let duplicate = Fixture::new("metadata-duplicate");
    duplicate.hook("PreToolUse", "duplicate-ai");
    fs::write(duplicate.repo.join("tracked.txt"), b"one\nsame\n").unwrap();
    duplicate.hook("PostToolUse", "duplicate-ai");
    fs::write(duplicate.repo.join("tracked.txt"), b"one\nsame\nsame\n").unwrap();
    duplicate.hook("PreToolUse", "duplicate-copy");
    duplicate.hook("PostToolUse", "duplicate-copy");
    let duplicate_commit = duplicate.commit("duplicate line copy");
    assert!(derive(&duplicate, &duplicate_commit).status.success());
    let duplicate_sidecar = sidecar_value(&duplicate, &duplicate_commit);
    let ambiguous = attribution_for(&duplicate_sidecar, "tracked.txt", "unknown");
    assert!(ambiguous["score"].is_null());
    assert!(ambiguous["limitingBasis"]
        .as_array()
        .unwrap()
        .contains(&json!("CONFLICTING_EVIDENCE")));
    duplicate.drop_root();

    let reconstructed = Fixture::new("metadata-reconstructed");
    reconstructed.hook("PreToolUse", "commit-tool");
    fs::write(
        reconstructed.repo.join("tracked.txt"),
        b"one\nreconstructed line\n",
    )
    .unwrap();
    let reconstructed_commit = reconstructed.commit("commit in window");
    reconstructed.hook("PostToolUse", "commit-tool");
    assert!(derive(&reconstructed, &reconstructed_commit)
        .status
        .success());
    let reconstructed_sidecar = sidecar_value(&reconstructed, &reconstructed_commit);
    let reconstructed_range = attribution_for(&reconstructed_sidecar, "tracked.txt", "ai_probable");
    assert_eq!(reconstructed_range["score"], 0.6);
    assert!(reconstructed_range["primaryBasis"].is_null());
    assert!(reconstructed_range["limitingBasis"]
        .as_array()
        .unwrap()
        .contains(&json!("X_RECONSTRUCTED_WINDOW")));
    reconstructed.drop_root();

    let mixed = Fixture::new("metadata-mixed");
    mixed.hook("PreToolUse", "mixed-source");
    fs::write(mixed.repo.join("tracked.txt"), b"one\nai line\n").unwrap();
    mixed.hook("PostToolUse", "mixed-source");
    fs::write(mixed.repo.join("tracked.txt"), b"one\nmodified line\n").unwrap();
    mixed.hook("PreToolUse", "modifier-tool");
    mixed.hook("PostToolUse", "modifier-tool");
    let mixed_commit = mixed.commit("mixed range");
    assert!(derive(&mixed, &mixed_commit).status.success());
    let mixed_sidecar = sidecar_value(&mixed, &mixed_commit);
    let mixed_range = attribution_for(&mixed_sidecar, "tracked.txt", "mixed");
    assert_eq!(mixed_range["score"], 0.84);
    assert_eq!(mixed_range["confidenceBand"], "high");
    assert_eq!(mixed_range["primaryBasis"], "EXACT_FRAGMENT_MATCH");
    assert!(mixed_range["limitingBasis"]
        .as_array()
        .unwrap()
        .contains(&json!("CONFLICTING_EVIDENCE")));
    assert_eq!(mixed_range["source"]["toolId"], "codex");
    assert_eq!(mixed_range["source"]["sessionRef"], "mixed-source");
    mixed.drop_root();

    let unbound = Fixture::new("metadata-unknown");
    fs::write(unbound.repo.join("tracked.txt"), b"one\nunbound line\n").unwrap();
    let unbound_commit = unbound.commit("unbound line");
    assert!(derive(&unbound, &unbound_commit).status.success());
    let unbound_sidecar = sidecar_value(&unbound, &unbound_commit);
    let unknown = attribution_for(&unbound_sidecar, "tracked.txt", "unknown");
    assert!(unknown["score"].is_null());
    assert_eq!(unknown["confidenceBand"], "none");
    assert!(unknown["primaryBasis"].is_null());
    assert_eq!(unknown["supportingBasis"], json!([]));
    assert_eq!(unknown["limitingBasis"], json!([]));
    unbound.drop_root();

    let excluded = Fixture::new("metadata-excluded");
    fs::write(excluded.repo.join("credentials.txt"), b"secret text\n").unwrap();
    let excluded_commit = excluded.commit("excluded file");
    assert!(derive(&excluded, &excluded_commit).status.success());
    let excluded_sidecar = sidecar_value(&excluded, &excluded_commit);
    let excluded_range = attribution_for(&excluded_sidecar, "credentials.txt", "excluded");
    assert!(excluded_range["score"].is_null());
    assert_eq!(excluded_range["confidenceBand"], "none");
    assert!(excluded_range["primaryBasis"].is_null());
    excluded.drop_root();

    let binary = Fixture::new("metadata-binary");
    fs::write(binary.repo.join("data.bin"), [b'x', 0, b'y', b'\n']).unwrap();
    let binary_commit = binary.commit("binary file");
    assert!(derive(&binary, &binary_commit).status.success());
    let binary_sidecar = sidecar_value(&binary, &binary_commit);
    let binary_range = attribution_for(&binary_sidecar, "data.bin", "excluded");
    assert!(binary_range["score"].is_null());
    assert_eq!(binary_range["confidenceBand"], "none");
    assert!(binary_range["limitingBasis"]
        .as_array()
        .unwrap()
        .contains(&json!("CONTENT_RESTRICTED")));
    binary.drop_root();
}

#[test]
fn time_cut_without_survival_eligibility_keeps_probable_fallback() {
    let fixture = Fixture::new("time-cut-survival-fallback");
    fixture.hook("PreToolUse", "ai-tool");
    fs::write(
        fixture.repo.join("tracked.txt"),
        b"one\ntwo\nlet stable_value = 12;\n",
    )
    .unwrap();
    fixture.hook("PostToolUse", "ai-tool");
    fs::write(
        fixture.repo.join("tracked.txt"),
        b"one\ntwo\nlet stable_value = 12;\nlet human_value = 4;\n",
    )
    .unwrap();
    fixture.hook("PreToolUse", "human-tool");
    fixture.hook("PostToolUse", "human-tool");
    fs::write(
        fixture.repo.join("tracked.txt"),
        b"one\ntwo\nlet stable_value = 12;\nlet human_value = 4;\nlate_value();\n",
    )
    .unwrap();
    let commit = fixture.commit("late edit after observed external step");
    assert!(derive(&fixture, &commit).status.success());
    let sidecar = sidecar_value(&fixture, &commit);
    let probable = attribution_for(&sidecar, "tracked.txt", "ai_probable");
    assert_eq!(probable["score"], 0.79);
    assert_eq!(probable["confidenceBand"], "medium");
    assert!(probable["limitingBasis"]
        .as_array()
        .unwrap()
        .contains(&json!("X_TIME_CUT")));
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
    // Before survival alignment, the file-wide time-cut cap made this AI line probable.
    // Now its exact surviving snapshot line stays strong and only the appended line is unknown.
    assert_eq!(tracked["ai_strong"], 1);
    assert_eq!(tracked["ai_probable"], 0);
    assert_eq!(tracked["mixed"], 0);
    assert_eq!(tracked["unknown"], 1);
    let sidecar = sidecar_value(&fixture, &commit);
    let survived = attribution_for(&sidecar, "tracked.txt", "ai_strong");
    assert!(survived["limitingBasis"]
        .as_array()
        .unwrap()
        .contains(&json!("X_UNOBSERVED_EDIT_IN_FILE")));
    let late = attribution_for(&sidecar, "tracked.txt", "unknown");
    assert!(late["limitingBasis"]
        .as_array()
        .unwrap()
        .contains(&json!("X_UNOBSERVED_EDIT")));
    fixture.drop_root();
}

#[test]
fn time_cut_material_edits_are_mixed_and_unrelated_replacements_unknown() {
    let fixture = Fixture::new("time-cut-material-mixed");
    fixture.hook("PreToolUse", "ai-tool");
    fs::write(
        fixture.repo.join("tracked.txt"),
        b"one\ntwo\nlet product = a * b;\nreturn a * b;\nlet stable_value = 12;\nfinish_task();\n",
    )
    .unwrap();
    fixture.hook("PostToolUse", "ai-tool");
    fs::write(
        fixture.repo.join("tracked.txt"),
        b"one\ntwo\nlet product = a * b;\nreturn a * c;\nlet stable_value = 12;\nfinish_task();\nnew_task();\n",
    )
    .unwrap();
    let commit = fixture.commit("human edits after AI capture");
    assert!(derive(&fixture, &commit).status.success());
    let report = fixture.derived_value(&commit);
    let tracked = file(&report, "tracked.txt");
    assert_eq!(tracked["ai_strong"], 3);
    assert_eq!(tracked["ai_probable"], 0);
    assert_eq!(tracked["mixed"], 1);
    assert_eq!(tracked["unknown"], 1);
    assert_eq!(tracked["e"], 5);
    let sidecar = sidecar_value(&fixture, &commit);
    let mixed = attribution_for(&sidecar, "tracked.txt", "mixed");
    assert_eq!(mixed["primaryBasis"], "CONFLICTING_EVIDENCE");
    assert_eq!(mixed["supportingBasis"], json!(["EXACT_FRAGMENT_MATCH"]));
    assert_eq!(mixed["score"], 0.84);
    assert!(mixed["limitingBasis"]
        .as_array()
        .unwrap()
        .contains(&json!("X_UNOBSERVED_EDIT")));
    fixture.drop_root();

    let unrelated = Fixture::new("time-cut-unrelated-replacement");
    unrelated.hook("PreToolUse", "ai-tool");
    fs::write(
        unrelated.repo.join("tracked.txt"),
        b"one\ntwo\ncalculate_total = left_value + right_value;\n",
    )
    .unwrap();
    unrelated.hook("PostToolUse", "ai-tool");
    fs::write(
        unrelated.repo.join("tracked.txt"),
        b"one\ntwo\nzebra orchard mountain window\n",
    )
    .unwrap();
    let unrelated_commit = unrelated.commit("unrelated replacement");
    assert!(derive(&unrelated, &unrelated_commit).status.success());
    let report = unrelated.derived_value(&unrelated_commit);
    let tracked = file(&report, "tracked.txt");
    assert_eq!(tracked["ai_strong"], 0);
    assert_eq!(tracked["mixed"], 0);
    assert_eq!(tracked["unknown"], 1);
    unrelated.drop_root();

    let deleted = Fixture::new("time-cut-deleted-ai");
    deleted.hook("PreToolUse", "ai-tool");
    fs::write(
        deleted.repo.join("tracked.txt"),
        b"one\ntwo\nfirst_ai_action();\nsecond_ai_action();\n",
    )
    .unwrap();
    deleted.hook("PostToolUse", "ai-tool");
    fs::write(
        deleted.repo.join("tracked.txt"),
        b"one\ntwo\nhuman_note();\n",
    )
    .unwrap();
    let deleted_commit = deleted.commit("delete AI lines");
    assert!(derive(&deleted, &deleted_commit).status.success());
    let report = deleted.derived_value(&deleted_commit);
    let tracked = file(&report, "tracked.txt");
    assert_eq!(tracked["ai_strong"], 0);
    assert_eq!(tracked["ai_probable"], 0);
    assert_eq!(tracked["mixed"], 0);
    assert_eq!(tracked["unknown"], 1);
    assert_eq!(tracked["e"], 1);
    assert_eq!(
        tracked["e"],
        tracked["ai_strong"].as_u64().unwrap()
            + tracked["ai_probable"].as_u64().unwrap()
            + tracked["mixed"].as_u64().unwrap()
            + tracked["unknown"].as_u64().unwrap()
    );
    deleted.drop_root();
}

#[test]
fn overlap_or_degraded_ai_events_are_probable() {
    for (name, field) in [("overlap", "overlap"), ("degraded", "degraded")] {
        let fixture = Fixture::new(name);
        fixture.hook("PreToolUse", "ai-tool");
        fs::write(fixture.repo.join("tracked.txt"), b"one\nai line\n").unwrap();
        fixture.hook("PostToolUse", "ai-tool");
        set_ai_change_field(&fixture, field, json!(true));
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
    assert_eq!(value["coverage"], "low");
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
fn missing_ai_snapshot_with_late_committed_edits_uses_time_cut_fallback() {
    let fixture = Fixture::new("missing-time-cut-snapshot");
    fixture.hook("PreToolUse", "ai-tool");
    fs::write(fixture.repo.join("tracked.txt"), b"one\nai line\n").unwrap();
    fixture.hook("PostToolUse", "ai-tool");
    fs::write(fixture.repo.join("tracked.txt"), b"one\nai line\nlate\n").unwrap();
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
    let ai_event = fs::read(&event_path)
        .unwrap()
        .split(|byte| *byte == b'\n')
        .filter_map(|line| serde_json::from_slice::<Value>(line).ok())
        .find(|event| event["event"] == "ai_change" && event["path"] == "tracked.txt")
        .unwrap();
    let snapshot = ai_event["after_sha"].as_str().unwrap();
    fs::remove_file(
        fixture
            .home
            .join("repos")
            .join(fixture.repo_id())
            .join("blobs")
            .join(snapshot),
    )
    .unwrap();

    let commit = fixture.commit("late edit with missing AI snapshot");
    assert!(derive(&fixture, &commit).status.success());
    let report = fixture.derived_value(&commit);
    let tracked = file(&report, "tracked.txt");
    assert_eq!(tracked["evidence"], "time_cut");
    assert_eq!(tracked["ai_probable"], 2);
    assert_eq!(tracked["ai_strong"], 0);
    let sidecar = sidecar_value(&fixture, &commit);
    let probable = attribution_for(&sidecar, "tracked.txt", "ai_probable");
    assert_eq!(probable["score"], 0.79);
    assert!(probable["limitingBasis"]
        .as_array()
        .unwrap()
        .contains(&json!("X_TIME_CUT")));
    fixture.drop_root();
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
    assert_eq!(file(&value, "renamed.txt")["unknown"], 0);
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
fn submodule_gitlinks_are_not_counted_as_added_text() {
    let fixture = Fixture::new("submodule-gitlink");
    let target = git_text(&fixture.repo, &["rev-parse", "HEAD"]);
    let cacheinfo = format!("160000,{target},vendor/module");
    git(
        &fixture.repo,
        &["update-index", "--add", "--cacheinfo", &cacheinfo],
    );
    git(&fixture.repo, &["commit", "-m", "add gitlink"]);
    let commit = git_text(&fixture.repo, &["rev-parse", "HEAD"]);
    assert!(derive(&fixture, &commit).status.success());
    let value = fixture.derived_value(&commit);
    assert!(value["files"]
        .as_array()
        .unwrap()
        .iter()
        .all(|entry| entry["path"] != "vendor/module"));
    assert_eq!(value["totals"]["e"], 0);
    assert_eq!(value["totals"]["excluded"], 0);
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
fn r28_duplicate_lines_fixture_attributes_verified_structured_positions_directly() {
    let _guard = FixtureTestGuard::new();
    let fixture_root =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/r28_duplicate_lines");
    let root = std::env::temp_dir().join(format!("prov-r28-{}-{}", std::process::id(), now()));
    let repo = root.join("repository");
    let home = root.join("store");
    fs::create_dir_all(&root).unwrap();
    let clone = isolated_command("git")
        .arg("clone")
        .arg(fixture_root.join("tasktrack-demo.bundle"))
        .arg(&repo)
        .output()
        .unwrap();
    assert!(
        clone.status.success(),
        "{}",
        String::from_utf8_lossy(&clone.stderr)
    );

    fn copy_tree(source: &Path, target: &Path) {
        fs::create_dir_all(target).unwrap();
        for entry in fs::read_dir(source).unwrap().flatten() {
            let from = entry.path();
            let to = target.join(entry.file_name());
            if from.is_dir() {
                copy_tree(&from, &to);
            } else {
                fs::copy(from, to).unwrap();
            }
        }
    }
    copy_tree(&fixture_root.join("repos"), &home.join("repos"));
    let expected: Value =
        serde_json::from_slice(&fs::read(fixture_root.join("expected-labels.json")).unwrap())
            .unwrap();
    let fixture = Fixture {
        root,
        repo,
        home,
        _test_guard: FixtureTestGuard::new(),
    };
    let commit = expected["commit"].as_str().unwrap();
    let result = derive(&fixture, commit);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let sidecar = fixture.sidecar_value(commit);
    for (path, lines) in expected["files"].as_object().unwrap() {
        let summary = fixture.cli(&[
            "summary",
            "--repo",
            fixture.repo.to_str().unwrap(),
            "--commit",
            commit,
            "--file",
            path,
            "--format",
            "json",
        ]);
        assert!(
            summary.status.success(),
            "{}",
            String::from_utf8_lossy(&summary.stderr)
        );
        let summary: Value = serde_json::from_slice(&summary.stdout).unwrap();
        for (line, category) in lines.as_object().unwrap() {
            let line: usize = line.parse().unwrap();
            let attribution = sidecar["attributions"]
                .as_array()
                .unwrap()
                .iter()
                .find(|range| {
                    range["path"] == *path
                        && range["startLine"].as_u64().unwrap() as usize <= line
                        && range["endLine"].as_u64().unwrap() as usize >= line
                })
                .unwrap_or_else(|| panic!("missing attribution for {path}:{line}"));
            assert_eq!(attribution["category"], *category, "{path}:{line}");
            assert!(
                summary["file_ranges"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|range| {
                        range["file"] == *path
                            && range["category"] == *category
                            && range["startLine"].as_u64().unwrap() as usize <= line
                            && range["endLine"].as_u64().unwrap() as usize >= line
                    }),
                "summary missed {path}:{line}"
            );

            let explanation = explain_json(&fixture, commit, path, Some(line));
            assert_eq!(
                explanation["lines"][0]["category"], *category,
                "{path}:{line}"
            );
            assert!(explanation["lines"][0]["checks"]
                .as_array()
                .unwrap()
                .iter()
                .any(
                    |check| check["condition"] == "duplicate_ambiguity" && check["passed"] == true
                ));
        }
    }
    fixture.drop_root();
}

#[test]
fn r30_codex_shell_after_reset_keeps_current_ai_evidence_over_discarded_patch_match() {
    let _guard = FixtureTestGuard::new();
    let fixture_root =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/r30_codex_shell_after_reset");
    let root = std::env::temp_dir().join(format!("prov-r30-{}-{}", std::process::id(), now()));
    let repo = root.join("repository");
    let home = root.join("store");
    fs::create_dir_all(&root).unwrap();
    let clone = isolated_command("git")
        .arg("clone")
        .arg(fixture_root.join("work.bundle"))
        .arg(&repo)
        .output()
        .unwrap();
    assert!(
        clone.status.success(),
        "{}",
        String::from_utf8_lossy(&clone.stderr)
    );

    fn copy_tree(source: &Path, target: &Path) {
        fs::create_dir_all(target).unwrap();
        for entry in fs::read_dir(source).unwrap().flatten() {
            let from = entry.path();
            let to = target.join(entry.file_name());
            if from.is_dir() {
                copy_tree(&from, &to);
            } else {
                fs::copy(from, to).unwrap();
            }
        }
    }
    copy_tree(&fixture_root.join("repos"), &home.join("repos"));
    let commit = fs::read_to_string(fixture_root.join("commits.txt"))
        .unwrap()
        .lines()
        .find_map(|line| {
            line.split_once(' ')
                .filter(|(_, message)| *message == "smoke test codex")
                .map(|(sha, _)| sha.to_owned())
        })
        .expect("fixture lists the reset-following shell commit");
    let fixture = Fixture {
        root,
        repo,
        home,
        _test_guard: FixtureTestGuard::new(),
    };
    let repo_id = fs::read_dir(fixture.home.join("repos"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .file_name();
    let repo_store = fixture.home.join("repos").join(repo_id);

    for suffix in ["json", "lines.json"] {
        let path = repo_store
            .join("commits")
            .join(format!("{commit}.{suffix}"));
        if path.exists() {
            fs::remove_file(path).unwrap();
        }
    }
    let result = derive(&fixture, &commit);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );

    let summary = fixture.cli(&[
        "summary",
        "--repo",
        fixture.repo.to_str().unwrap(),
        "--commit",
        &commit,
        "--file",
        "smoke_test.txt",
        "--format",
        "json",
    ]);
    assert!(
        summary.status.success(),
        "{}",
        String::from_utf8_lossy(&summary.stderr)
    );
    let summary: Value = serde_json::from_slice(&summary.stdout).unwrap();
    assert_eq!(summary["totals"]["ai_strong"], 3);
    assert_eq!(summary["totals"]["unknown"], 0);
    assert_eq!(summary["totals"]["coverage"], "high");

    let explanation = explain_json(&fixture, &commit, "smoke_test.txt", None);
    for line in explanation["lines"].as_array().unwrap() {
        assert_eq!(line["category"], "ai_strong");
        assert_eq!(line["score"], 0.92);
        assert_eq!(line["firstFailedCondition"], "structured_edit");
    }
    fixture.drop_root();
}

#[test]
fn reset_without_identical_committed_text_keeps_new_shell_lines_strong() {
    let fixture = Fixture::new("reset-distinct-content");
    let parent = git_text(&fixture.repo, &["rev-parse", "HEAD"]);
    fs::write(
        fixture.repo.join("smoke_test.txt"),
        b"discarded one\ndiscarded two\n",
    )
    .unwrap();
    let discarded = fixture.commit("discarded distinct text");
    assert!(derive(&fixture, &discarded).status.success());

    git(&fixture.repo, &["reset", "--hard", &parent]);
    fixture.hook("PreToolUse", "reset-fresh-shell");
    fs::write(
        fixture.repo.join("smoke_test.txt"),
        b"fresh one\nfresh two\n",
    )
    .unwrap();
    fixture.hook("PostToolUse", "reset-fresh-shell");
    let target = fixture.commit("fresh shell text after reset");
    assert!(derive(&fixture, &target).status.success());
    let sidecar = fixture.sidecar_value(&target);
    assert!(sidecar["files"][0]["lines"]
        .as_array()
        .unwrap()
        .iter()
        .all(|line| line["label"] == "s"));
    fixture.drop_root();
}

#[test]
fn identical_committed_text_without_reset_keeps_current_shell_lines_strong() {
    let fixture = Fixture::new("identical-text-no-reset");
    let parent = git_text(&fixture.repo, &["rev-parse", "HEAD"]);
    git(&fixture.repo, &["switch", "-c", "discarded-content-source"]);
    fs::write(
        fixture.repo.join("smoke_test.txt"),
        b"identical one\nidentical two\n",
    )
    .unwrap();
    let discarded = fixture.commit("unobserved identical content");
    assert!(derive(&fixture, &discarded).status.success());

    git(&fixture.repo, &["switch", "--detach", &parent]);
    fixture.hook("PreToolUse", "identical-shell-current");
    fs::write(
        fixture.repo.join("smoke_test.txt"),
        b"identical one\nidentical two\n",
    )
    .unwrap();
    fixture.hook("PostToolUse", "identical-shell-current");
    let target = fixture.commit("same text with current shell evidence");
    assert!(derive(&fixture, &target).status.success());
    let sidecar = fixture.sidecar_value(&target);
    assert!(sidecar["files"][0]["lines"]
        .as_array()
        .unwrap()
        .iter()
        .all(|line| line["label"] == "s"));
    fixture.drop_root();
}

#[test]
fn explain_duplicate_ambiguity_lists_the_competing_ai_event() {
    let fixture = Fixture::new("explain-duplicate-candidates");
    fixture.hook("PreToolUse", "duplicate-source");
    fs::write(fixture.repo.join("tracked.txt"), b"base\nsame line\n").unwrap();
    fixture.hook("PostToolUse", "duplicate-source");
    fixture.hook("PreToolUse", "duplicate-copy");
    fs::write(
        fixture.repo.join("tracked.txt"),
        b"base\nsame line\nsame line\n",
    )
    .unwrap();
    fixture.hook("PostToolUse", "duplicate-copy");
    mark_event_external(&fixture, "duplicate-copy");
    let commit = fixture.commit("explain duplicate candidate");
    assert!(derive(&fixture, &commit).status.success());

    let explanation = explain_json(&fixture, &commit, "tracked.txt", Some(3));
    let checks = explanation["lines"][0]["checks"].as_array().unwrap();
    let duplicate = checks
        .iter()
        .find(|check| check["condition"] == "duplicate_ambiguity")
        .unwrap();
    assert_eq!(duplicate["passed"], false);
    let candidates: Value = serde_json::from_str(
        duplicate["compared"]
            .as_str()
            .unwrap()
            .split_once("competing_candidates=")
            .unwrap()
            .1,
    )
    .unwrap();
    assert!(candidates.as_array().unwrap().iter().any(|candidate| {
        candidate["tool_use_id"] == "duplicate-source"
            && candidate["session"] == "derive-test-session"
            && candidate["event_index"].is_number()
            && candidate["why_competes"]
                .as_str()
                .is_some_and(|reason| reason.contains("identical text"))
    }));
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
    let base = git_text(&fixture.repo, &["rev-parse", "HEAD"]);
    let first = fixture.commits(1).remove(0);
    git(&fixture.repo, &["checkout", &base]);
    fs::write(fixture.repo.join("tracked.txt"), b"one\ntwo\nsibling\n").unwrap();
    let second = fixture.commit("sibling pending commit");
    let commits = vec![first, second];
    fixture.queue_head_moves(&commits);
    let repo_store = fixture.home.join("repos").join(fixture.repo_id());
    let commits_dir = repo_store.join("commits");
    fs::create_dir_all(&commits_dir).unwrap();
    fixture.remove_loose_tree_object(&commits[0]);

    let result = fixture.pending();
    assert_eq!(
        result.status.code(),
        Some(0),
        "pending returned nonzero; commits: {}",
        commits_listing(&commits_dir)
    );
    assert!(
        result.stdout.is_empty() && result.stderr.is_empty(),
        "pending was not silent; commits: {}",
        commits_listing(&commits_dir)
    );
    assert!(
        !fixture.derived_path(&commits[0]).exists(),
        "failed commit report exists; commits: {}",
        commits_listing(&commits_dir)
    );
    assert!(
        commits_dir.join(format!("{}.attempt", commits[0])).exists(),
        "attempt marker missing; commits: {}",
        commits_listing(&commits_dir)
    );
    assert!(
        fixture.derived_path(&commits[1]).exists(),
        "later commit report missing; commits: {}",
        commits_listing(&commits_dir)
    );
    assert!(
        !repo_store.join("derive.lock").exists(),
        "derive lock remains; commits: {}",
        commits_listing(&commits_dir)
    );
    fixture.drop_root();
}

#[test]
fn derive_fails_when_sidecar_destination_is_a_directory() {
    let fixture = Fixture::new("sidecar-directory");
    fixture.hook("Stop", "sidecar-directory");
    fs::write(
        fixture.repo.join("tracked.txt"),
        b"one\ntwo\nblocked sidecar content\n",
    )
    .unwrap();
    let commit = fixture.commit("blocked sidecar destination");
    let sidecar_path = fixture.lines_path(&commit);
    fs::create_dir_all(&sidecar_path).unwrap();

    let result = derive(&fixture, &commit);
    let commits_dir = sidecar_path.parent().unwrap();
    assert!(!result.status.success());
    assert!(
        !fixture.derived_path(&commit).exists(),
        "commit report written despite sidecar failure; commits: {}",
        commits_listing(commits_dir)
    );
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
    assert_eq!(report["coverage"], "high");
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
    let carried_range = sidecar["attributions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|range| {
            range["path"] == "tracked.txt"
                && range["category"] == "ai_probable"
                && range["startLine"] == 3
        })
        .unwrap();
    assert_eq!(carried_range["score"], 0.60);
    assert_eq!(carried_range["confidenceBand"], "medium");
    assert_eq!(carried_range["primaryBasis"], "MOVE_LINEAGE");
    assert_eq!(carried_range["source"]["adapterId"], "codex");
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
    assert_eq!(report["rewrite"]["method"], "none");
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
    assert_eq!(report["rewrite"]["method"], "none");
    assert_eq!(report["rewrite"]["carried_lines"], 0);
    assert_eq!(line_label(&sidecar, "tracked.txt", &line_hash(shared)), "u");
    assert_eq!(report["rewrite"]["sources"], json!([]));
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

#[test]
fn unrelated_source_with_one_common_line_fails_overlap_threshold() {
    let fixture = Fixture::new("rewrite-overlap-filter");
    let base = git_text(&fixture.repo, &["rev-parse", "HEAD"]);
    let common = "import numpy as np";
    git(&fixture.repo, &["checkout", "-b", "unrelated-ai"]);
    let source = ai_commit(
        &fixture,
        &format!("one\ntwo\n{common}\nsource-only alpha\nsource-only beta\n"),
        "unrelated AI source",
    );

    git(&fixture.repo, &["checkout", &base]);
    git(&fixture.repo, &["checkout", "-b", "target-branch"]);
    fixture.clear_events();
    fs::write(
        fixture.repo.join("tracked.txt"),
        format!("one\ntwo\n{common}\ntarget-only phrase\n"),
    )
    .unwrap();
    let target = fixture.commit("unrelated target");
    assert!(derive(&fixture, &target).status.success());

    let report = fixture.derived_value(&target);
    let sidecar = fixture.sidecar_value(&target);
    assert_eq!(report["rewrite"]["method"], "none");
    assert_eq!(report["rewrite"]["sources"], json!([]));
    assert_eq!(report["rewrite"]["carried_lines"], 0);
    assert_eq!(line_label(&sidecar, "tracked.txt", &line_hash(common)), "u");
    assert_ne!(source, target);
    fixture.drop_root();
}

#[test]
fn rewrite_scan_with_two_hundred_twenty_derived_commits_stays_bounded() {
    let fixture = Fixture::new("rewrite-bounded-scan");
    fixture.hook("Stop", "large-rewrite-init");
    let base = git_text(&fixture.repo, &["rev-parse", "HEAD"]);
    let noise = fixture.empty_commits(220);
    let range = format!("{base}..{}", noise.last().unwrap());
    let derived_noise = fixture.cli(&[
        "derive",
        "--repo",
        fixture.repo.to_str().unwrap(),
        "--range",
        &range,
    ]);
    assert!(
        derived_noise.status.success(),
        "{}",
        String::from_utf8_lossy(&derived_noise.stderr)
    );

    let noise_tip = noise.last().unwrap();
    git(&fixture.repo, &["checkout", "-b", "bounded-ai-source"]);
    let phrase = "AI source phrase survives bounded scanning";
    let source = ai_commit(
        &fixture,
        &format!("one\ntwo\n{phrase}\n"),
        "AI source among many reports",
    );
    git(&fixture.repo, &["checkout", noise_tip]);
    git(&fixture.repo, &["checkout", "-b", "bounded-rewrite"]);
    fs::write(fixture.repo.join("other.txt"), b"distinct rewrite base\n").unwrap();
    let _ = fixture.commit("distinct rewrite base");
    git(&fixture.repo, &["cherry-pick", &source]);
    let rewritten = git_text(&fixture.repo, &["rev-parse", "HEAD"]);

    let started = Instant::now();
    let result = derive(&fixture, &rewritten);
    let elapsed = started.elapsed();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "rewrite derive took {elapsed:?}"
    );
    let report = fixture.derived_value(&rewritten);
    let sidecar = fixture.sidecar_value(&rewritten);
    assert_eq!(
        report["rewrite"]["method"], "patch_id",
        "source={source}, target={rewritten}, report={report}"
    );
    assert_eq!(report["rewrite"]["sources"], json!([source]));
    assert_eq!(line_label(&sidecar, "tracked.txt", &line_hash(phrase)), "s");
    fixture.drop_root();
}

fn prior_ai_origin(fixture: &Fixture, structured: bool) -> String {
    let original = "alpha beta gamma delta";
    fixture.hook("PreToolUse", "prior-ai-origin");
    fs::write(
        fixture.repo.join("tracked.txt"),
        format!("{original}\ntwo\n"),
    )
    .unwrap();
    fixture.hook("PostToolUse", "prior-ai-origin");
    configure_structured_apply_patch(
        fixture,
        "tracked.txt",
        Some(structured),
        false,
        json!([[1, 1]]),
    );
    let origin = fixture.commit("AI writes origin line");
    assert!(derive(fixture, &origin).status.success());
    origin
}

#[test]
fn prior_ai_line_human_edits_carry_mixed_for_direct_and_strong_origins() {
    for (name, structured, expected_category) in [
        ("prior-ai-direct", true, "ai_direct"),
        ("prior-ai-strong", false, "ai_strong"),
    ] {
        let fixture = Fixture::new(name);
        let origin = prior_ai_origin(&fixture, structured);
        assert_eq!(
            file(&fixture.derived_value(&origin), "tracked.txt")[expected_category],
            1
        );
        // The un-derived bridge makes the target depend on the line's actual blame origin.
        git(
            &fixture.repo,
            &["commit", "--allow-empty", "-m", "un-derived bridge"],
        );

        fs::write(
            fixture.repo.join("tracked.txt"),
            b"alpha beta gamma epsilon\ntwo\n",
        )
        .unwrap();
        let edit = fixture.commit("human changes one word");
        assert!(derive(&fixture, &edit).status.success());
        let report = fixture.derived_value(&edit);
        let sidecar = fixture.sidecar_value(&edit);
        assert_eq!(file(&report, "tracked.txt")["mixed"], 1);
        assert_eq!(file(&report, "tracked.txt")["unknown"], 0);
        let mixed = attribution_for(&sidecar, "tracked.txt", "mixed");
        assert_eq!(
            mixed["primaryBasis"], "HUMAN_EDIT_OF_PRIOR_AI_LINE",
            "{sidecar}"
        );
        assert_eq!(mixed["score"], 0.84);
        assert_eq!(mixed["priorAiOrigin"]["originCommit"], origin);
        assert_eq!(mixed["priorAiOrigin"]["originLine"], 1);
        assert_eq!(mixed["priorAiOrigin"]["originCategory"], expected_category);
        assert!(mixed["priorAiOrigin"]["overlap"].as_f64().unwrap() >= 0.5);

        let explain = explain_json(&fixture, &edit, "tracked.txt", Some(1));
        let line = &explain["lines"][0];
        assert_eq!(line["category"], "mixed");
        let checks = line["checks"].as_array().unwrap();
        let prior_gate = checks
            .iter()
            .find(|check| check["condition"] == "prior_ai_line_edit")
            .unwrap();
        let prior_detail = prior_gate["compared"].as_str().unwrap();
        assert!(prior_detail.contains(&origin));
        assert!(prior_detail.contains(expected_category));
        assert!(prior_detail.contains("overlap="));
        let final_gate = checks
            .iter()
            .find(|check| check["condition"] == "final_attribution")
            .unwrap();
        assert!(final_gate["compared"]
            .as_str()
            .unwrap()
            .contains("decision=carry_human_edit_of_prior_ai"));

        let summary = fixture.cli(&[
            "summary",
            "--repo",
            fixture.repo.to_str().unwrap(),
            "--commit",
            &edit,
            "--file",
            "tracked.txt",
            "--format",
            "json",
        ]);
        assert!(summary.status.success());
        let summary: Value = serde_json::from_slice(&summary.stdout).unwrap();
        let summary_range = summary["file_ranges"]
            .as_array()
            .unwrap()
            .iter()
            .find(|range| range["commit"] == edit && range["category"] == "mixed")
            .unwrap_or_else(|| panic!("summary has no carried range: {summary}"));
        assert_eq!(summary_range["primaryBasis"], "HUMAN_EDIT_OF_PRIOR_AI_LINE");
        assert_eq!(summary_range["priorAiOrigin"]["originCommit"], origin);

        let repeated = derive(&fixture, &edit);
        assert!(repeated.status.success());
        assert_eq!(fixture.derived_value(&edit), report);
        assert_eq!(fixture.sidecar_value(&edit), sidecar);
        fixture.drop_root();
    }
}

#[test]
fn prior_ai_line_rewrite_stays_unknown_and_missing_origin_is_explained() {
    let rewritten = Fixture::new("prior-ai-rewrite");
    let _origin = prior_ai_origin(&rewritten, true);
    fs::write(
        rewritten.repo.join("tracked.txt"),
        b"unrelated replacement\ntwo\n",
    )
    .unwrap();
    let edit = rewritten.commit("human rewrites line");
    assert!(derive(&rewritten, &edit).status.success());
    assert_eq!(
        file(&rewritten.derived_value(&edit), "tracked.txt")["unknown"],
        1
    );
    let explain = explain_json(&rewritten, &edit, "tracked.txt", Some(1));
    assert_eq!(explain["lines"][0]["category"], "unknown");
    let rewrite_gate = explain["lines"][0]["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["condition"] == "prior_ai_line_edit")
        .unwrap();
    assert!(rewrite_gate["compared"]
        .as_str()
        .unwrap()
        .contains("below min_mixed_overlap"));
    rewritten.drop_root();

    let missing = Fixture::new("prior-ai-missing-origin");
    let origin = prior_ai_origin(&missing, true);
    fs::remove_file(missing.derived_path(&origin)).unwrap();
    fs::remove_file(missing.lines_path(&origin)).unwrap();
    fs::write(
        missing.repo.join("tracked.txt"),
        b"alpha beta gamma epsilon\ntwo\n",
    )
    .unwrap();
    let edit = missing.commit("human edits without stored origin");
    assert!(derive(&missing, &edit).status.success());
    assert_eq!(
        file(&missing.derived_value(&edit), "tracked.txt")["unknown"],
        1
    );
    let explain = explain_json(&missing, &edit, "tracked.txt", Some(1));
    let missing_gate = explain["lines"][0]["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["condition"] == "prior_ai_line_edit")
        .unwrap();
    assert!(missing_gate["compared"]
        .as_str()
        .unwrap()
        .contains("origin commit not derived locally"));
    missing.drop_root();
}

#[test]
fn prior_ai_line_blank_replacement_and_pure_insertion_stay_unknown() {
    let fixture = Fixture::new("prior-ai-blank-insert");
    fs::write(fixture.repo.join("tracked.txt"), b"\nbase phrase stays\n").unwrap();
    let _scaffold = fixture.commit("add blank and base line");
    fixture.hook("PreToolUse", "prior-ai-blank-source");
    fs::write(
        fixture.repo.join("tracked.txt"),
        b"\nalpha beta gamma delta\n",
    )
    .unwrap();
    fixture.hook("PostToolUse", "prior-ai-blank-source");
    configure_structured_apply_patch(&fixture, "tracked.txt", Some(true), false, json!([[2, 2]]));
    let origin = fixture.commit("AI adds prior line");
    assert!(derive(&fixture, &origin).status.success());

    fs::write(
        fixture.repo.join("tracked.txt"),
        b"new text\nalpha beta gamma delta\npure insertion\n",
    )
    .unwrap();
    let edit = fixture.commit("blank replacement and insertion");
    assert!(derive(&fixture, &edit).status.success());
    let sidecar = fixture.sidecar_value(&edit);
    assert_eq!(
        line_label(&sidecar, "tracked.txt", &line_hash("new text")),
        "u"
    );
    assert_eq!(
        line_label(&sidecar, "tracked.txt", &line_hash("pure insertion")),
        "u"
    );
    fixture.drop_root();
}

#[test]
fn prior_ai_mixed_origin_chains_and_own_window_evidence_wins() {
    let fixture = Fixture::new("prior-ai-mixed-chain");
    let _origin = prior_ai_origin(&fixture, true);
    git(
        &fixture.repo,
        &["commit", "--allow-empty", "-m", "un-derived bridge"],
    );
    fs::write(
        fixture.repo.join("tracked.txt"),
        b"alpha beta gamma epsilon\ntwo\n",
    )
    .unwrap();
    let first_edit = fixture.commit("human edits direct AI line");
    assert!(derive(&fixture, &first_edit).status.success());

    git(
        &fixture.repo,
        &["commit", "--allow-empty", "-m", "second un-derived bridge"],
    );
    fs::write(
        fixture.repo.join("tracked.txt"),
        b"alpha beta gamma zeta\ntwo\n",
    )
    .unwrap();
    let second_edit = fixture.commit("human edits carried mixed line");
    assert!(derive(&fixture, &second_edit).status.success());
    let sidecar = fixture.sidecar_value(&second_edit);
    let mixed = attribution_for(&sidecar, "tracked.txt", "mixed");
    assert_eq!(mixed["priorAiOrigin"]["originCommit"], first_edit);
    assert_eq!(mixed["priorAiOrigin"]["originCategory"], "mixed");

    fixture.hook("PreToolUse", "same-window-ai");
    fs::write(
        fixture.repo.join("tracked.txt"),
        b"alpha beta gamma eta\ntwo\n",
    )
    .unwrap();
    fixture.hook("PostToolUse", "same-window-ai");
    configure_structured_apply_patch(&fixture, "tracked.txt", Some(true), false, json!([[1, 1]]));
    let own_window = fixture.commit("AI evidence in current window");
    assert!(derive(&fixture, &own_window).status.success());
    assert_eq!(
        file(&fixture.derived_value(&own_window), "tracked.txt")["ai_direct"],
        1
    );
    fixture.drop_root();
}

#[test]
fn prior_ai_synthetic_real_case_shape_preserves_linked_and_deterministic_percentages() {
    let fixture = Fixture::new("prior-ai-48-line-shape");
    fixture.hook("PreToolUse", "shape-cross-origin");
    fs::write(
        fixture.repo.join("cross.txt"),
        b"alpha beta gamma delta\nomega psi chi phi\n",
    )
    .unwrap();
    fixture.hook("PostToolUse", "shape-cross-origin");
    configure_structured_apply_patch(&fixture, "cross.txt", Some(true), false, json!([[1, 2]]));
    let origin = fixture.commit("synthetic cross-file AI source");
    assert!(derive(&fixture, &origin).status.success());
    git(
        &fixture.repo,
        &["commit", "--allow-empty", "-m", "un-derived bridge"],
    );

    fixture.hook("PreToolUse", "shape-direct-source");
    let direct = (0..26)
        .map(|index| format!("direct symbol {index} alpha"))
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(fixture.repo.join("direct.txt"), format!("{direct}\n")).unwrap();
    fixture.hook("PostToolUse", "shape-direct-source");
    configure_structured_apply_patch(&fixture, "direct.txt", Some(true), false, json!([[1, 26]]));

    fixture.hook("PreToolUse", "shape-strong-source");
    let strong = (0..18)
        .map(|index| format!("strong symbol {index} beta"))
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(fixture.repo.join("strong.txt"), format!("{strong}\n")).unwrap();
    fixture.hook("PostToolUse", "shape-strong-source");
    configure_structured_apply_patch(&fixture, "strong.txt", Some(false), false, json!([[1, 18]]));

    fs::write(
        fixture.repo.join("cross.txt"),
        b"alpha beta gamma epsilon\nomega psi chi tau\n",
    )
    .unwrap();
    fs::write(
        fixture.repo.join("unknown.txt"),
        b"rewrite unknown\npure insert\n",
    )
    .unwrap();
    let target = fixture.commit("synthetic mixed cross-commit attribution shape");
    assert!(derive(&fixture, &target).status.success());
    let report = fixture.derived_value(&target);
    assert_eq!(report["totals"]["e"], 48);
    assert_eq!(report["totals"]["ai_direct"], 26);
    assert_eq!(report["totals"]["ai_strong"], 18);
    assert_eq!(report["totals"]["mixed"], 2);
    assert_eq!(report["totals"]["unknown"], 2);
    assert_eq!(report["totals"]["ai_deterministic_pct"], 91.7);
    assert_eq!(report["totals"]["ai_linked_pct"], 95.8);
    assert_eq!(report["coverage_unobserved_lines"], 2);
    assert_eq!(file(&report, "direct.txt")["ai_direct"], 26);
    assert_eq!(file(&report, "strong.txt")["ai_strong"], 18);
    assert_ne!(origin, target);
    fixture.drop_root();
}
