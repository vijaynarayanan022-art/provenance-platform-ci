use serde_json::{json, Value};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
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

struct Fixture {
    root: PathBuf,
    repo: PathBuf,
    store: PathBuf,
    copilot_home: PathBuf,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("prov-vscode-{name}-{}-{nonce}", std::process::id()));
        let repo = root.join("repository with spaces");
        let store = root.join("capture store");
        let copilot_home = root.join("copilot home");
        fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init"]);
        git(&repo, &["config", "user.name", "VS Code Adapter Test"]);
        git(&repo, &["config", "user.email", "vscode@example.invalid"]);
        fs::write(repo.join("tracked.txt"), "base\n").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-m", "initial"]);
        Self {
            root,
            repo,
            store,
            copilot_home,
        }
    }

    fn cli(&self, args: &[&str]) -> Output {
        isolated_command(env!("CARGO_BIN_EXE_prov-hook"))
            .args(args)
            .env("PROV_HOME", &self.store)
            .env("PROV_NO_AUTODERIVE", "1")
            .env("COPILOT_HOME", &self.copilot_home)
            .output()
            .unwrap()
    }

    fn hook(&self, payload: &Value, adapter: &str) -> Output {
        let mut child = isolated_command(env!("CARGO_BIN_EXE_prov-hook"))
            .arg(adapter)
            .env("PROV_HOME", &self.store)
            .env("PROV_NO_AUTODERIVE", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(&serde_json::to_vec(payload).unwrap())
            .unwrap();
        child.wait_with_output().unwrap()
    }

    fn fixture(&self, name: &str) -> Value {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/vscode")
            .join(format!("{name}.json"));
        serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
    }

    fn repo_payload(&self, name: &str) -> Value {
        let mut payload = self.fixture(name);
        payload["cwd"] = json!(fs::canonicalize(&self.repo).unwrap_or_else(|_| self.repo.clone()));
        payload["session_id"] = json!("vscode-test-session");
        payload
    }

    fn base(&self) -> String {
        git_text(&self.repo, &["rev-parse", "HEAD"])
    }

    fn commit(&self, message: &str) -> String {
        git(&self.repo, &["add", "-A"]);
        git(&self.repo, &["commit", "-m", message]);
        git_text(&self.repo, &["rev-parse", "HEAD"])
    }

    fn drop_root(&self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn git(repo: &Path, args: &[&str]) {
    let output = isolated_command("git")
        .args(args)
        .current_dir(repo)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_AUTHOR_NAME", "VS Code Adapter Test")
        .env("GIT_AUTHOR_EMAIL", "vscode@example.invalid")
        .env("GIT_COMMITTER_NAME", "VS Code Adapter Test")
        .env("GIT_COMMITTER_EMAIL", "vscode@example.invalid")
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

fn assert_hook_response(output: &Output) {
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(output.stdout, b"{}");
    assert!(output.stderr.is_empty());
}

fn events(home: &Path) -> Vec<Value> {
    let mut all = Vec::new();
    let mut pending = vec![home.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let Ok(entries) = fs::read_dir(directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else if path
                .extension()
                .is_some_and(|extension| extension == "jsonl")
            {
                for line in fs::read(path).unwrap().split(|byte| *byte == b'\n') {
                    if let Ok(value) = serde_json::from_slice(line) {
                        all.push(value);
                    }
                }
            }
        }
    }
    all
}

fn normalized_path(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap().join(path)
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
    let normalized: PathBuf = parts.into_iter().collect();
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

fn event_file_diagnostics(home: &Path) -> String {
    let mut pending = vec![home.to_path_buf()];
    let mut files = Vec::new();
    while let Some(directory) = pending.pop() {
        let Ok(entries) = fs::read_dir(directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else if path
                .extension()
                .is_some_and(|extension| extension == "jsonl")
            {
                let contents = fs::read_to_string(&path).unwrap_or_default();
                files.push(format!("{}:\n{contents}", path.display()));
            }
        }
    }
    files.sort();
    if files.is_empty() {
        "<no event files>".into()
    } else {
        files.join("\n")
    }
}

fn failure_diagnostics(fixture: &Fixture, raw_target: &str, report: &Value) -> String {
    format!(
        "payload cwd={:?}\nnormalized cwd={:?}\nrepo root={:?}\nnormalized repo root={:?}\npayload filePath/patch path={raw_target:?}\nnormalized target={:?}\nevent files:\n{}\nreport JSON:\n{}",
        fs::canonicalize(&fixture.repo).unwrap_or_else(|_| fixture.repo.clone()),
        normalized_path(&fixture.repo),
        fixture.repo,
        fs::canonicalize(&fixture.repo).unwrap_or_else(|_| fixture.repo.clone()),
        normalized_path(Path::new(raw_target)),
        event_file_diagnostics(&fixture.store),
        serde_json::to_string_pretty(report).unwrap_or_default(),
    )
}

fn file_report(fixture: &Fixture, commit: &str) -> Value {
    let repo_id = fs::read_dir(fixture.store.join("repos"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .file_name()
        .to_string_lossy()
        .into_owned();
    serde_json::from_slice(
        &fs::read(
            fixture
                .store
                .join("repos")
                .join(repo_id)
                .join("commits")
                .join(format!("{commit}.json")),
        )
        .unwrap(),
    )
    .unwrap()
}

#[test]
fn vscode_always_returns_exact_empty_object_for_empty_garbage_and_fixtures() {
    let fixture = Fixture::new("stdout");
    let executable = env!("CARGO_BIN_EXE_prov-hook");
    for payload in [Vec::new(), b"not json".to_vec()] {
        let mut child = isolated_command(executable)
            .arg("vscode")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(&payload).unwrap();
        assert_hook_response(&child.wait_with_output().unwrap());
    }
    for name in [
        "session-start",
        "pre-create-file",
        "post-create-file",
        "pre-apply-patch",
        "post-apply-patch",
        "post-file-search",
        "post-get-errors",
        "pre-run-in-terminal",
        "post-run-in-terminal",
        "stop",
        "pre-unsupported-tool",
    ] {
        let payload = fixture.repo_payload(name);
        assert_hook_response(&fixture.hook(&payload, "vscode"));
    }
    fixture.drop_root();
}

#[test]
fn read_only_tools_are_ignored_and_unknown_tool_logs_diagnostic_and_opens_window() {
    let fixture = Fixture::new("filtering");
    for name in [
        "file_search",
        "get_errors",
        "grep_search",
        "list_dir",
        "read_file",
        "memory",
        "vscode_askQuestions",
        "semantic_search",
    ] {
        for event in ["PreToolUse", "PostToolUse"] {
            let payload = json!({
                "timestamp": "2026-11-04T19:46:50.000Z",
                "hook_event_name": event,
                "session_id": "read-only-session",
                "transcript_path": "ignored",
                "cwd": fixture.repo,
                "tool_name": name,
                "tool_input": {},
                "tool_use_id": format!("{name}-{event}"),
                "tool_response": "",
            });
            assert_hook_response(&fixture.hook(&payload, "vscode"));
        }
    }
    assert!(events(&fixture.store).is_empty());

    let pre = fixture.repo_payload("pre-unsupported-tool");
    assert_hook_response(&fixture.hook(&pre, "vscode"));
    let mut post = pre.clone();
    post["hook_event_name"] = json!("PostToolUse");
    post["tool_response"] = json!("");
    assert_hook_response(&fixture.hook(&post, "vscode"));
    let markers = fs::read_to_string(
        fs::read_dir(fixture.store.join("repos"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path()
            .join("markers.jsonl"),
    )
    .unwrap();
    let diagnostics: Vec<Value> = markers
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .filter(|value: &Value| value["event"] == "unsupported_tool")
        .collect();
    assert_eq!(
        diagnostics,
        [json!({"event":"unsupported_tool","tool_name":"unknown_editor_tool"})]
    );
    assert!(events(&fixture.store)
        .iter()
        .any(|event| event["event"] == "window_open"));
    assert!(events(&fixture.store)
        .iter()
        .any(|event| event["event"] == "window_close"));
    fixture.drop_root();
}

#[test]
fn r26_store_fixture_identifies_the_unknown_existing_file_edit_tool() {
    let fixture_root =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/vscode_edit_tools_r26");
    let markers: Vec<Value> = fs::read_to_string(fixture_root.join("markers.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let unsupported: Vec<_> = markers
        .iter()
        .filter(|marker| marker["event"] == "unsupported_tool")
        .collect();
    assert_eq!(unsupported.len(), 2);
    assert!(unsupported
        .iter()
        .all(|marker| marker["tool_name"] == "multi_replace_string_in_file"));

    let events: Vec<Value> = fs::read_to_string(fixture_root.join("copilot-events.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    for path in ["index.js", "package.json"] {
        assert!(events.iter().any(|event| {
            event["event"] == "external_change"
                && event["path"] == path
                && event["hook_event_name"] == "PreToolUse"
        }));
    }
    assert!(events
        .iter()
        .any(|event| { event["event"] == "ai_change" && event["path"] == "taskUtils.js" }));
}

#[test]
fn r26_commit_27_multi_replace_edits_are_direct_for_existing_files() {
    let fixture = Fixture::new("r26-commit-27");
    let fixture_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/vscode_edit_tools_r26/commit-27");
    let edits = [("package.json", "package"), ("index.js", "index")];
    for (path, stem) in edits {
        let before = fs::read(fixture_root.join(format!("{stem}.before.bin"))).unwrap();
        fs::write(fixture.repo.join(path), &before).unwrap();
    }
    fixture.commit("R26 parent files");

    for (index, (path, stem)) in edits.iter().enumerate() {
        let before = fs::read(fixture_root.join(format!("{stem}.before.bin"))).unwrap();
        let after = fs::read(fixture_root.join(format!("{stem}.after.bin"))).unwrap();
        let input = json!({
            "replacements":[{
                "filePath":fixture.repo.join(path),
                "oldString":String::from_utf8(before).unwrap(),
                "newString":String::from_utf8(after.clone()).unwrap()
            }]
        });
        let pre = json!({
            "hook_event_name":"PreToolUse","session_id":"r26-session",
            "cwd":fixture.repo,"tool_name":"multi_replace_string_in_file",
            "tool_use_id":format!("r26-edit-{index}"),"tool_input":input
        });
        assert_hook_response(&fixture.hook(&pre, "vscode"));
        fs::write(fixture.repo.join(path), after).unwrap();
        let mut post = pre;
        post["hook_event_name"] = json!("PostToolUse");
        assert_hook_response(&fixture.hook(&post, "vscode"));
        if index == 0 {
            let terminal_pre = json!({
                "hook_event_name":"PreToolUse","session_id":"r26-session",
                "cwd":fixture.repo,"tool_name":"run_in_terminal","tool_use_id":"test-run",
                "tool_input":{"command":"npm test"}
            });
            assert_hook_response(&fixture.hook(&terminal_pre, "vscode"));
            let mut terminal_post = terminal_pre;
            terminal_post["hook_event_name"] = json!("PostToolUse");
            assert_hook_response(&fixture.hook(&terminal_post, "vscode"));
        }
    }

    let commit = fixture.commit("R26 source changes");
    let derived = fixture.cli(&[
        "derive",
        "--repo",
        fixture.repo.to_str().unwrap(),
        "--commit",
        &commit,
    ]);
    assert!(
        derived.status.success(),
        "{}",
        String::from_utf8_lossy(&derived.stderr)
    );
    let report = file_report(&fixture, &commit);
    let summary = fixture.cli(&[
        "summary",
        "--repo",
        fixture.repo.to_str().unwrap(),
        "--commit",
        &commit,
        "--format",
        "json",
    ]);
    assert!(
        summary.status.success(),
        "{}",
        String::from_utf8_lossy(&summary.stderr)
    );
    let summary: Value = serde_json::from_slice(&summary.stdout).unwrap();
    assert!(summary["totals"]["ai_direct"].as_u64().unwrap() > 0);
    for path in ["index.js", "package.json"] {
        let file = report["files"]
            .as_array()
            .unwrap()
            .iter()
            .find(|file| file["path"] == path)
            .unwrap();
        assert!(file["ai_direct"].as_u64().unwrap() > 0, "{path}: {file}");
        let explanation = fixture.cli(&[
            "explain",
            "--repo",
            fixture.repo.to_str().unwrap(),
            "--commit",
            &commit,
            "--file",
            path,
            "--format",
            "json",
        ]);
        assert!(
            explanation.status.success(),
            "{path}: {}",
            String::from_utf8_lossy(&explanation.stderr)
        );
        let explanation: Value = serde_json::from_slice(&explanation.stdout).unwrap();
        assert!(
            explanation["lines"]
                .as_array()
                .unwrap()
                .iter()
                .any(|line| { line["category"] == "ai_direct" }),
            "{path}: {explanation}"
        );
    }
    fixture.drop_root();
}

#[test]
fn recognized_existing_file_edit_tools_are_direct_and_unknown_tools_are_strong() {
    let recognized = [
        (
            "insert_edit_into_file",
            json!({"filePath":"tracked.txt","code":"const added = true;\n"}),
            "base\nconst added = true\n",
        ),
        (
            "replace_string_in_file",
            json!({"filePath":"tracked.txt","oldString":"base","newString":"changed\nconst added = true"}),
            "changed\nconst added = true\n",
        ),
        (
            "multi_replace_string_in_file",
            json!({"replacements":[{"filePath":"tracked.txt","oldString":"base","newString":"changed\nconst added = true"}]}),
            "changed\nconst added = true\n",
        ),
        (
            "apply_patch",
            json!({"input":"*** Begin Patch\n*** Update File: tracked.txt\n@@\n base\n+const added = true;\n*** End Patch"}),
            "base\nconst added = true;\n",
        ),
    ];
    for (tool, input, after) in recognized {
        let fixture = Fixture::new(tool);
        let mut pre = json!({
            "hook_event_name":"PreToolUse","session_id":"edit-session",
            "cwd":fixture.repo,"tool_name":tool,"tool_use_id":"edit-call",
            "tool_input":input
        });
        if let Some(path) = pre["tool_input"]["filePath"].as_str() {
            pre["tool_input"]["filePath"] = json!(fixture.repo.join(path));
        }
        if let Some(path) = pre["tool_input"]["replacements"][0]["filePath"].as_str() {
            pre["tool_input"]["replacements"][0]["filePath"] = json!(fixture.repo.join(path));
        }
        assert_hook_response(&fixture.hook(&pre, "vscode"));
        fs::write(fixture.repo.join("tracked.txt"), after).unwrap();
        let mut post = pre.clone();
        post["hook_event_name"] = json!("PostToolUse");
        assert_hook_response(&fixture.hook(&post, "vscode"));

        let matching = events(&fixture.store)
            .into_iter()
            .find(|event| event["event"] == "ai_change" && event["path"] == "tracked.txt")
            .unwrap_or_else(|| panic!("{tool} did not record an AI change"));
        assert_eq!(matching["structured_edit"], true, "{tool}");
        assert!(matching["before_sha"].is_string(), "{tool}");
        assert!(matching["after_sha"].is_string(), "{tool}");
        assert!(matching["line_ranges"].is_array(), "{tool}");

        let commit = fixture.commit("Copilot edit");
        let derived = fixture.cli(&[
            "derive",
            "--repo",
            fixture.repo.to_str().unwrap(),
            "--commit",
            &commit,
        ]);
        assert!(
            derived.status.success(),
            "{tool}: {}",
            String::from_utf8_lossy(&derived.stderr)
        );
        let report = file_report(&fixture, &commit);
        let file = report["files"]
            .as_array()
            .unwrap()
            .iter()
            .find(|file| file["path"] == "tracked.txt")
            .unwrap();
        assert!(file["ai_direct"].as_u64().unwrap() > 0, "{tool}: {file}");
        fixture.drop_root();
    }

    let fixture = Fixture::new("unknown-edit-fallback");
    let pre = json!({
        "hook_event_name":"PreToolUse","session_id":"unknown-session",
        "cwd":fixture.repo,"tool_name":"future_edit_tool","tool_use_id":"unknown-call",
        "tool_input":{"filePath":fixture.repo.join("tracked.txt")}
    });
    assert_hook_response(&fixture.hook(&pre, "vscode"));
    fs::write(fixture.repo.join("tracked.txt"), "base\nfallback line\n").unwrap();
    let mut post = pre.clone();
    post["hook_event_name"] = json!("PostToolUse");
    assert_hook_response(&fixture.hook(&post, "vscode"));
    let ai_event = events(&fixture.store)
        .into_iter()
        .find(|event| event["event"] == "ai_change" && event["path"] == "tracked.txt")
        .unwrap();
    assert_eq!(ai_event["structured_edit"], false);
    let markers = events(&fixture.store);
    assert!(markers
        .iter()
        .any(|event| event["event"] == "unsupported_tool"
            && event["tool_name"] == "future_edit_tool"));
    let commit = fixture.commit("Unknown tool edit");
    let derived = fixture.cli(&[
        "derive",
        "--repo",
        fixture.repo.to_str().unwrap(),
        "--commit",
        &commit,
    ]);
    assert!(derived.status.success());
    let file = file_report(&fixture, &commit)["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|file| file["path"] == "tracked.txt")
        .unwrap()
        .clone();
    assert_eq!(file["ai_direct"], 0);
    assert!(file["ai_strong"].as_u64().unwrap() > 0);
    fixture.drop_root();
}

#[test]
fn create_file_and_patch_replay_derive_exact_vscode_labels_and_report_tool() {
    let fixture = Fixture::new("exact-labels");
    let base = fixture.base();
    let canonical_repo = fs::canonicalize(&fixture.repo).unwrap();
    let hello = canonical_repo.join("hello.py");
    let mut target_path = fixture
        .repo
        .join("nested")
        .join("..")
        .join("hello.py")
        .to_string_lossy()
        .into_owned();
    if target_path.as_bytes().get(1) == Some(&b':') {
        let lower_drive = target_path[0..1].to_ascii_lowercase();
        target_path.replace_range(0..1, &lower_drive);
    }

    let mut pre_create = fixture.repo_payload("pre-create-file");
    pre_create["tool_input"]["filePath"] = json!(target_path);
    assert_hook_response(&fixture.hook(&pre_create, "vscode"));
    fs::write(&hello, "def add(a, b):\n    return a + b\n").unwrap();
    let mut post_create = fixture.repo_payload("post-create-file");
    post_create["tool_input"]["filePath"] = json!(target_path);
    assert_hook_response(&fixture.hook(&post_create, "vscode"));

    let mut pre_patch = fixture.repo_payload("pre-apply-patch");
    let patch = pre_patch["tool_input"]["input"].as_str().unwrap();
    pre_patch["tool_input"]["input"] =
        json!(patch.replace("c:\\dev\\hookcheck\\lab2\\hello.py", &target_path));
    assert_hook_response(&fixture.hook(&pre_patch, "vscode"));
    fs::write(
        &hello,
        "def add(a, b):\n    return a + b\n\n\ndef subtract(a, b):\n    return a - b\n",
    )
    .unwrap();
    let mut post_patch = fixture.repo_payload("post-apply-patch");
    let patch = post_patch["tool_input"]["input"].as_str().unwrap();
    post_patch["tool_input"]["input"] =
        json!(patch.replace("c:\\dev\\hookcheck\\lab2\\hello.py", &target_path));
    assert_hook_response(&fixture.hook(&post_patch, "vscode"));

    let commit = fixture.commit("Copilot edits");
    let derived = fixture.cli(&[
        "derive",
        "--repo",
        fixture.repo.to_str().unwrap(),
        "--commit",
        &commit,
    ]);
    assert!(
        derived.status.success(),
        "{}",
        String::from_utf8_lossy(&derived.stderr)
    );
    let report = file_report(&fixture, &commit);
    let hello_report = report["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|file| file["path"] == "hello.py")
        .unwrap();
    let diagnostics = failure_diagnostics(&fixture, &target_path, &report);
    assert_eq!(hello_report["evidence"], "exact", "{diagnostics}");
    assert_eq!(hello_report["ai_direct"], 4, "{diagnostics}");
    assert_eq!(hello_report["ai_strong"], 0, "{diagnostics}");
    assert_eq!(hello_report["ai_probable"], 0, "{diagnostics}");
    assert_eq!(hello_report["unknown"], 0, "{diagnostics}");

    let pr = fixture.cli(&[
        "pr-report",
        "--repo",
        fixture.repo.to_str().unwrap(),
        "--base",
        &base,
        "--head",
        &commit,
    ]);
    assert!(
        pr.status.success(),
        "{}",
        String::from_utf8_lossy(&pr.stderr)
    );
    let pr: Value = serde_json::from_slice(&pr.stdout).unwrap();
    assert!(pr["tools"]
        .as_array()
        .unwrap()
        .contains(&json!("vscode-copilot")));
    fixture.drop_root();
}

#[test]
fn missing_pre_snapshot_is_degraded_and_never_ai_credited() {
    let fixture = Fixture::new("missing-pre");
    fs::write(
        fixture.repo.join("tracked.txt"),
        "base\nchanged without pre\n",
    )
    .unwrap();
    let mut post = fixture.repo_payload("post-create-file");
    post["cwd"] = json!(fixture.root);
    post["tool_input"]["filePath"] = json!(fixture.repo.join("tracked.txt"));
    post["tool_use_id"] = json!("missing-pre-id");
    assert_hook_response(&fixture.hook(&post, "vscode"));
    let matching = events(&fixture.store)
        .into_iter()
        .find(|event| event["hook_event_name"] == "PostToolUse" && event["path"] == "tracked.txt")
        .unwrap();
    assert_eq!(matching["event"], "external_change");
    assert_eq!(matching["reason"], "before_unavailable");
    assert_eq!(matching["degraded"], true);
    assert!(matching["line_ranges"].is_null());
    let commit = fixture.commit("edit without pre snapshot");
    let derived = fixture.cli(&[
        "derive",
        "--repo",
        fixture.repo.to_str().unwrap(),
        "--commit",
        &commit,
    ]);
    assert!(
        derived.status.success(),
        "{}",
        String::from_utf8_lossy(&derived.stderr)
    );
    let report = file_report(&fixture, &commit);
    let tracked = report["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|file| file["path"] == "tracked.txt")
        .unwrap();
    assert_eq!(tracked["ai_strong"], 0);
    assert!(tracked["unknown"].as_u64().unwrap() >= 1);
    fixture.drop_root();
}

#[test]
fn human_edit_between_tool_calls_stays_external_and_unattributed() {
    let fixture = Fixture::new("human-between-tools");
    let input = json!({
        "filePath":fixture.repo.join("tracked.txt"),
        "oldString":"base",
        "newString":"base\nAI line"
    });
    let first_pre = json!({
        "hook_event_name":"PreToolUse","session_id":"human-session",
        "cwd":fixture.repo,"tool_name":"replace_string_in_file",
        "tool_use_id":"first-edit","tool_input":input
    });
    assert_hook_response(&fixture.hook(&first_pre, "vscode"));
    let mut first_post = first_pre.clone();
    first_post["hook_event_name"] = json!("PostToolUse");
    assert_hook_response(&fixture.hook(&first_post, "vscode"));

    fs::write(fixture.repo.join("tracked.txt"), "human line\n").unwrap();
    let second_pre = json!({
        "hook_event_name":"PreToolUse","session_id":"human-session",
        "cwd":fixture.repo,"tool_name":"replace_string_in_file",
        "tool_use_id":"second-edit","tool_input":input
    });
    assert_hook_response(&fixture.hook(&second_pre, "vscode"));
    fs::write(fixture.repo.join("tracked.txt"), "human line\nAI line\n").unwrap();
    let mut second_post = second_pre.clone();
    second_post["hook_event_name"] = json!("PostToolUse");
    assert_hook_response(&fixture.hook(&second_post, "vscode"));

    let records = events(&fixture.store);
    assert!(records.iter().any(|event| {
        event["event"] == "external_change"
            && event["path"] == "tracked.txt"
            && event["hook_event_name"] == "PreToolUse"
    }));
    let commit = fixture.commit("human and AI edits");
    let derived = fixture.cli(&[
        "derive",
        "--repo",
        fixture.repo.to_str().unwrap(),
        "--commit",
        &commit,
    ]);
    assert!(derived.status.success());
    let file = file_report(&fixture, &commit)["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|file| file["path"] == "tracked.txt")
        .unwrap()
        .clone();
    assert_eq!(file["ai_direct"], 1);
    assert_eq!(file["unknown"], 1);
    fixture.drop_root();
}

#[test]
fn terminal_changes_match_codex_shell_path_evidence() {
    let fixture = Fixture::new("terminal-vscode");
    let initial_blob = git_text(&fixture.repo, &["rev-parse", "HEAD:tracked.txt"]);
    let pre = fixture.repo_payload("pre-run-in-terminal");
    assert_hook_response(&fixture.hook(&pre, "vscode"));
    fs::write(fixture.repo.join("tracked.txt"), "base\nterminal line\n").unwrap();
    let post = fixture.repo_payload("post-run-in-terminal");
    assert_hook_response(&fixture.hook(&post, "vscode"));
    let commit = fixture.commit("terminal edit");
    assert!(fixture
        .cli(&[
            "derive",
            "--repo",
            fixture.repo.to_str().unwrap(),
            "--commit",
            &commit
        ])
        .status
        .success());
    let vscode_report = file_report(&fixture, &commit);
    let vscode_blob = git_text(&fixture.repo, &["rev-parse", "HEAD:tracked.txt"]);
    let vscode_diagnostics = failure_diagnostics(
        &fixture,
        fixture.repo.join("tracked.txt").to_string_lossy().as_ref(),
        &vscode_report,
    );
    assert_ne!(initial_blob, vscode_blob, "{vscode_diagnostics}");
    let vscode_event = events(&fixture.store)
        .into_iter()
        .find(|event| event["event"] == "ai_change" && event["path"] == "tracked.txt")
        .unwrap_or_else(|| panic!("{vscode_diagnostics}"));
    assert_eq!(
        vscode_event["tool"], "vscode-copilot",
        "{vscode_diagnostics}"
    );
    let vscode_file = vscode_report["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|file| file["path"] == "tracked.txt")
        .unwrap();
    assert_eq!(vscode_file["ai_direct"], 0);
    assert!(vscode_file["ai_strong"].as_u64().unwrap() > 0);

    let codex = Fixture::new("terminal-codex");
    let codex_initial_blob = git_text(&codex.repo, &["rev-parse", "HEAD:tracked.txt"]);
    let pre = json!({
        "hook_event_name":"PreToolUse","session_id":"codex-shell-session","turn_id":"t",
        "cwd":codex.repo,"tool_name":"Bash","tool_use_id":"shell-1","tool_input":{"command":"python make_file.py"}
    });
    let output = codex.hook(&pre, "codex");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stdout.is_empty() && output.stderr.is_empty());
    fs::write(codex.repo.join("tracked.txt"), "base\nterminal line\n").unwrap();
    let mut post = pre.clone();
    post["hook_event_name"] = json!("PostToolUse");
    let output = codex.hook(&post, "codex");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stdout.is_empty() && output.stderr.is_empty());
    let codex_commit = codex.commit("terminal edit");
    assert!(codex
        .cli(&[
            "derive",
            "--repo",
            codex.repo.to_str().unwrap(),
            "--commit",
            &codex_commit,
        ])
        .status
        .success());
    let codex_report = file_report(&codex, &codex_commit);
    let codex_blob = git_text(&codex.repo, &["rev-parse", "HEAD:tracked.txt"]);
    let codex_diagnostics = failure_diagnostics(
        &codex,
        &codex.repo.join("tracked.txt").to_string_lossy(),
        &codex_report,
    );
    assert_ne!(codex_initial_blob, codex_blob, "{codex_diagnostics}");
    let codex_file = codex_report["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|file| file["path"] == "tracked.txt")
        .unwrap_or_else(|| panic!("{codex_diagnostics}"));
    assert_eq!(codex_file["ai_direct"], 0);
    assert!(codex_file["ai_strong"].as_u64().unwrap() > 0);
    let vscode_file = file_report(&fixture, &commit)["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|file| file["path"] == "tracked.txt")
        .unwrap_or_else(|| panic!("{vscode_diagnostics}"))
        .clone();
    for field in ["ai_strong", "ai_probable", "mixed", "unknown", "evidence"] {
        assert_eq!(
            vscode_file[field],
            codex_file[field],
            "field {field}\nVS Code diagnostics:\n{vscode_diagnostics}\nCodex diagnostics:\n{codex_diagnostics}"
        );
    }
    fixture.drop_root();
    codex.drop_root();
}

#[test]
fn setup_is_idempotent_preserves_foreign_entries_and_doctor_checks_binary() {
    let fixture = Fixture::new("setup");
    let hooks_file = fixture.copilot_home.join("hooks").join("prov-hook.json");
    fs::create_dir_all(hooks_file.parent().unwrap()).unwrap();
    fs::write(
        &hooks_file,
        r#"{"version":1,"other":"keep","hooks":{"preToolUse":[{"type":"command","command":"other"}]}}"#,
    )
    .unwrap();
    let dry_run = fixture.cli(&["install", "--tool", "vscode", "--dry-run"]);
    assert!(dry_run.status.success());
    assert_eq!(
        fs::read(&hooks_file).unwrap(),
        br#"{"version":1,"other":"keep","hooks":{"preToolUse":[{"type":"command","command":"other"}]}}"#
    );
    let installed = fixture.cli(&["setup", "--tool", "vscode"]);
    assert!(
        installed.status.success(),
        "{}",
        String::from_utf8_lossy(&installed.stderr)
    );
    let first = fs::read(&hooks_file).unwrap();
    let document: Value = serde_json::from_slice(&first).unwrap();
    assert_eq!(document["version"], 1);
    assert_eq!(document["other"], "keep");
    for event in ["preToolUse", "postToolUse", "agentStop"] {
        let entries = document["hooks"][event].as_array().unwrap();
        let ours: Vec<_> = entries
            .iter()
            .filter(|entry| {
                entry["powershell"]
                    .as_str()
                    .is_some_and(|value| value.ends_with(" vscode"))
            })
            .collect();
        assert_eq!(ours.len(), 1);
        assert_eq!(ours[0]["type"], "command");
        assert_eq!(ours[0]["timeoutSec"], 5);
        assert!(ours[0]["bash"].as_str().unwrap().ends_with(" vscode"));
    }
    assert_eq!(document["hooks"]["preToolUse"].as_array().unwrap().len(), 2);
    let repeated = fixture.cli(&["install", "--tool", "vscode"]);
    assert!(repeated.status.success());
    assert_eq!(fs::read(&hooks_file).unwrap(), first);
    let dry_uninstall = fixture.cli(&["uninstall", "--tool", "vscode", "--dry-run"]);
    assert!(dry_uninstall.status.success());
    assert_eq!(fs::read(&hooks_file).unwrap(), first);

    let doctor = fixture.cli(&["doctor", "--tool", "vscode"]);
    assert!(
        doctor.status.success(),
        "{}",
        String::from_utf8_lossy(&doctor.stdout)
    );
    let doctor = String::from_utf8(doctor.stdout).unwrap();
    assert_eq!(doctor.matches("PASS ").count(), 3);

    let removed = fixture.cli(&["uninstall", "--tool", "vscode"]);
    assert!(
        removed.status.success(),
        "{}",
        String::from_utf8_lossy(&removed.stderr)
    );
    let document: Value = serde_json::from_slice(&fs::read(&hooks_file).unwrap()).unwrap();
    assert_eq!(document["other"], "keep");
    assert_eq!(
        document["hooks"]["preToolUse"],
        json!([{"type":"command","command":"other"}])
    );
    assert!(document["hooks"]["postToolUse"]
        .as_array()
        .unwrap()
        .is_empty());
    let failed_doctor = fixture.cli(&["doctor", "--tool", "vscode"]);
    assert!(!failed_doctor.status.success());
    assert!(String::from_utf8_lossy(&failed_doctor.stdout).contains("FAIL command path"));
    fixture.drop_root();
}

#[test]
fn vscode_doctor_reports_missing_and_invalid_hooks_file() {
    let fixture = Fixture::new("doctor-invalid");
    let missing = fixture.cli(&["doctor", "--tool", "vscode"]);
    assert!(!missing.status.success());
    assert_eq!(
        String::from_utf8_lossy(&missing.stdout)
            .matches("FAIL ")
            .count(),
        3
    );

    let hooks_file = fixture.copilot_home.join("hooks").join("prov-hook.json");
    fs::create_dir_all(hooks_file.parent().unwrap()).unwrap();
    fs::write(&hooks_file, "not json").unwrap();
    let invalid = fixture.cli(&["doctor", "--tool", "vscode"]);
    assert!(!invalid.status.success());
    let output = String::from_utf8_lossy(&invalid.stdout);
    assert!(output.contains("PASS hooks file present"));
    assert!(output.contains("FAIL valid JSON"));
    assert!(output.contains("FAIL command path"));
    fixture.drop_root();
}
