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
    codex_home: PathBuf,
    copilot_home: PathBuf,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "prov-commands-{name}-{}-{nonce}",
            std::process::id()
        ));
        let repo = root.join("repository with spaces");
        let store = root.join("capture store");
        let codex_home = root.join("Codex Home");
        let copilot_home = root.join("Copilot Home");
        fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init"]);
        git(&repo, &["config", "user.name", "Command Test"]);
        git(&repo, &["config", "user.email", "commands@example.invalid"]);
        fs::write(repo.join("tracked.txt"), "base\n").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-m", "initial"]);
        Self {
            root,
            repo,
            store,
            codex_home,
            copilot_home,
        }
    }

    fn cli(&self, args: &[&str]) -> Output {
        self.cli_at(&self.repo, args)
    }

    fn cli_at(&self, cwd: &Path, args: &[&str]) -> Output {
        isolated_command(env!("CARGO_BIN_EXE_prov-hook"))
            .args(args)
            .current_dir(cwd)
            .env("PROV_HOME", &self.store)
            .env("PROV_NO_AUTODERIVE", "1")
            .env("CODEX_HOME", &self.codex_home)
            .env("COPILOT_HOME", &self.copilot_home)
            .env("LOCALAPPDATA", self.root.join("Local AppData"))
            .env_remove("GIT_CONFIG_COUNT")
            .env_remove("GIT_CONFIG_KEY_0")
            .env_remove("GIT_CONFIG_VALUE_0")
            .output()
            .unwrap()
    }

    fn hook(&self, adapter: &str, payload: &Value) {
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
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success());
        if adapter == "vscode" {
            assert_eq!(output.stdout, b"{}");
        } else {
            assert!(output.stdout.is_empty());
        }
        assert!(output.stderr.is_empty());
    }

    fn base(&self) -> String {
        git_text(&self.repo, &["rev-parse", "HEAD"])
    }

    fn commit(&self, message: &str) -> String {
        git(&self.repo, &["add", "-A"]);
        git(&self.repo, &["commit", "-m", message]);
        git_text(&self.repo, &["rev-parse", "HEAD"])
    }

    fn derive(&self, commit: &str) {
        let output = self.cli(&[
            "derive",
            "--repo",
            self.repo.to_str().unwrap(),
            "--commit",
            commit,
        ]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn codex_edit(&self, id: &str, content: &str, message: &str) -> String {
        let pre = json!({
            "hook_event_name":"PreToolUse","session_id":"commands-codex-session",
            "turn_id":"turn","cwd":self.repo,"tool_name":"Bash","tool_use_id":id,
            "tool_input":{"command":"edit tracked.txt"}
        });
        self.hook("codex", &pre);
        fs::write(self.repo.join("tracked.txt"), content).unwrap();
        let mut post = pre;
        post["hook_event_name"] = json!("PostToolUse");
        self.hook("codex", &post);
        let commit = self.commit(message);
        self.derive(&commit);
        commit
    }

    fn vscode_edit(&self, id: &str, content: &str, message: &str) -> String {
        let target = self.repo.join("tracked.txt");
        let pre = json!({
            "timestamp":"2026-11-05T00:00:00Z","hook_event_name":"PreToolUse",
            "session_id":"commands-vscode-session","transcript_path":"ignored",
            "cwd":self.repo,"tool_name":"create_file","tool_use_id":id,
            "tool_input":{"filePath":target,"content":content}
        });
        self.hook("vscode", &pre);
        fs::write(&target, content).unwrap();
        let mut post = pre;
        post["hook_event_name"] = json!("PostToolUse");
        post["tool_response"] = json!("");
        self.hook("vscode", &post);
        let commit = self.commit(message);
        self.derive(&commit);
        commit
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
        .env("GIT_AUTHOR_NAME", "Command Test")
        .env("GIT_AUTHOR_EMAIL", "commands@example.invalid")
        .env("GIT_COMMITTER_NAME", "Command Test")
        .env("GIT_COMMITTER_EMAIL", "commands@example.invalid")
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

fn parsed(output: &Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn commit_report(fixture: &Fixture, commit: &str) -> Value {
    let repo_store = fs::read_dir(fixture.store.join("repos"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    serde_json::from_slice(
        &fs::read(repo_store.join("commits").join(format!("{commit}.json"))).unwrap(),
    )
    .unwrap()
}

#[test]
fn enable_disable_are_idempotent_and_leave_capture_data_untouched() {
    let fixture = Fixture::new("enable-disable");
    let github_hook = fixture.repo.join(".github/hooks/foreign-hook");
    fs::create_dir_all(github_hook.parent().unwrap()).unwrap();
    fs::write(&github_hook, b"keep me\n").unwrap();
    let repo_id = git_text(&fixture.repo, &["rev-list", "--max-parents=0", "HEAD"]);
    let data = fixture
        .store
        .join("repos")
        .join(repo_id)
        .join("events")
        .join("preserve.jsonl");
    fs::create_dir_all(data.parent().unwrap()).unwrap();
    fs::write(&data, b"recorded data\n").unwrap();

    for _ in 0..2 {
        let output = fixture.cli(&[
            "enable",
            "--repo",
            fixture.repo.to_str().unwrap(),
            "--tools",
            "codex,vscode",
        ]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("PASS codex:"));
        assert!(stdout.contains("PASS vscode:"));
        assert!(stdout.contains("PASS git:"));
        assert!(stdout.contains("DOCTOR codex:"));
        assert!(stdout.contains("DOCTOR vscode:"));
        assert!(stdout.contains("PASS hooks feature:"));
    }
    assert!(fixture.codex_home.join("config.toml").is_file());
    assert!(fixture
        .copilot_home
        .join("hooks")
        .join("prov-hook.json")
        .is_file());
    for hook in ["post-commit", "post-rewrite"] {
        assert!(fixture.repo.join(".git/hooks").join(hook).is_file());
    }
    assert_eq!(fs::read(&github_hook).unwrap(), b"keep me\n");

    for _ in 0..2 {
        let output = fixture.cli(&[
            "disable",
            "--repo",
            fixture.repo.to_str().unwrap(),
            "--tools",
            "codex,vscode",
        ]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("PASS codex:"));
        assert!(stdout.contains("PASS vscode:"));
        assert!(stdout.contains("PASS git:"));
    }
    assert_eq!(fs::read(data).unwrap(), b"recorded data\n");
    assert_eq!(fs::read(&github_hook).unwrap(), b"keep me\n");
    assert!(!fixture
        .copilot_home
        .join("hooks")
        .join("prov-hook.json")
        .exists());
    fixture.drop_root();
}

#[test]
fn enable_dry_run_writes_nothing_and_reports_planned_paths() {
    let fixture = Fixture::new("dry-run");
    let output = fixture.cli(&[
        "enable",
        "--repo",
        fixture.repo.to_str().unwrap(),
        "--dry-run",
    ]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("PASS codex: would install "));
    assert!(stdout.contains("PASS vscode: would install "));
    assert!(stdout.contains("PASS git: would write "));
    assert!(!fixture.codex_home.join("config.toml").exists());
    assert!(!fixture
        .copilot_home
        .join("hooks")
        .join("prov-hook.json")
        .exists());
    for hook in ["post-commit", "post-rewrite"] {
        assert!(!fixture.repo.join(".git/hooks").join(hook).exists());
    }
    let disable = fixture.cli(&[
        "disable",
        "--repo",
        fixture.repo.to_str().unwrap(),
        "--dry-run",
    ]);
    assert!(disable.status.success());
    assert!(String::from_utf8_lossy(&disable.stdout).contains("PASS git: would remove "));
    assert!(!fixture.codex_home.join("config.toml").exists());
    assert!(!fixture
        .copilot_home
        .join("hooks")
        .join("prov-hook.json")
        .exists());
    fixture.drop_root();
}

#[test]
fn enable_without_repository_skips_only_git_hooks() {
    let fixture = Fixture::new("no-repo");
    let outside = fixture.root.join("outside repository");
    fs::create_dir_all(&outside).unwrap();
    let output = fixture.cli_at(&outside, &["enable", "--tools", "codex,vscode"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("PASS codex:"));
    assert!(stdout.contains("PASS vscode:"));
    assert!(stdout.contains("SKIPPED git: <no repository>"));
    assert!(fixture.codex_home.join("config.toml").is_file());
    assert!(fixture
        .copilot_home
        .join("hooks")
        .join("prov-hook.json")
        .is_file());
    fixture.drop_root();
}

#[test]
fn enable_continues_after_one_install_step_fails() {
    let fixture = Fixture::new("partial-failure");
    fs::create_dir_all(&fixture.codex_home).unwrap();
    fs::write(fixture.codex_home.join("config.toml"), "PreToolUse = []\n").unwrap();
    let output = fixture.cli(&[
        "enable",
        "--repo",
        fixture.repo.to_str().unwrap(),
        "--tools",
        "codex,vscode",
    ]);
    assert!(!output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("FAIL codex:"));
    assert!(stdout.contains("PASS vscode:"));
    assert!(stdout.contains("PASS git:"));
    assert!(stdout.contains("DOCTOR codex:"));
    assert!(fixture
        .copilot_home
        .join("hooks")
        .join("prov-hook.json")
        .is_file());
    assert!(fixture.repo.join(".git/hooks/post-commit").is_file());
    fixture.drop_root();
}

#[test]
fn summary_range_matches_pr_report_and_expands_full_category_names() {
    let fixture = Fixture::new("summary-parity");
    let base = fixture.base();
    fixture.codex_edit("codex-one", "base\ncodex line\n", "codex contribution");
    let vscode_commit = fixture.vscode_edit(
        "vscode-one",
        "base\ncodex line\nvscode line\n",
        "vscode contribution",
    );
    fs::write(
        fixture.repo.join("tracked.txt"),
        "base\ncodex line\nvscode line\nhuman line\n",
    )
    .unwrap();
    let head = fixture.commit("human contribution");
    fixture.derive(&head);

    let summary = fixture.cli(&[
        "summary",
        "--repo",
        fixture.repo.to_str().unwrap(),
        "--range",
        &format!("{base}..{head}"),
        "--file",
        "tracked.txt",
        "--format",
        "json",
    ]);
    let summary: Value = parsed(&summary);
    assert_eq!(summary["schema"], "prov.summary/0.1");
    assert!(summary["created_at"].as_str().is_some());
    assert!(summary["versions"].is_object());
    assert_eq!(summary["versions"]["line_survival"], "unobserved-mixed-v1");
    assert_eq!(summary["versions"]["min_mixed_overlap"], 0.5);
    assert_eq!(summary["commits"].as_array().unwrap().len(), 3);
    assert!(summary["commits"]
        .as_array()
        .unwrap()
        .iter()
        .any(|commit| commit["commit"] == vscode_commit));
    let report = fixture.cli(&[
        "pr-report",
        "--repo",
        fixture.repo.to_str().unwrap(),
        "--base",
        &base,
        "--head",
        &head,
        "--no-derive",
    ]);
    let report: Value = parsed(&report);
    for field in [
        "eligible",
        "ai_direct",
        "ai_strong",
        "ai_probable",
        "mixed",
        "unknown",
        "excluded",
    ] {
        assert_eq!(
            summary["totals"][field], report["lines"][field],
            "field {field}"
        );
    }
    assert_eq!(
        summary["totals"]["deterministic_pct"],
        report["ai_deterministic_pct"]
    );
    assert_eq!(summary["totals"]["linked_pct"], report["ai_linked_pct"]);
    assert_eq!(summary["totals"]["coverage"], report["coverage"]["band"]);
    assert_eq!(
        summary["totals"]["tools"],
        json!(["codex", "vscode-copilot"])
    );
    let file_ranges = summary["file_ranges"].as_array().unwrap();
    assert!(file_ranges
        .iter()
        .any(|line| line["category"] == "ai_strong"));
    assert!(file_ranges.iter().any(|line| line["category"] == "unknown"));
    assert!(file_ranges.iter().all(|line| {
        matches!(
            line["category"].as_str(),
            Some("ai_direct" | "ai_strong" | "ai_probable" | "mixed" | "unknown" | "excluded")
        )
    }));
    assert!(file_ranges
        .iter()
        .all(|line| line["category"].as_str().unwrap().len() > 1));
    let codex_range = file_ranges
        .iter()
        .find(|range| range["source"]["adapterId"] == "codex")
        .unwrap();
    let vscode_range = file_ranges
        .iter()
        .find(|range| range["source"]["adapterId"] == "vscode-copilot")
        .unwrap();
    assert_eq!(codex_range["category"], "ai_strong");
    assert_eq!(codex_range["score"], 0.92);
    assert_eq!(codex_range["confidenceBand"], "high");
    assert_eq!(codex_range["primaryBasis"], "EXACT_FRAGMENT_MATCH");
    assert_eq!(codex_range["source"]["toolId"], "codex");
    assert_eq!(codex_range["source"]["sessionRef"], "codex-one");
    assert_eq!(codex_range["lines"], 1);
    assert_eq!(vscode_range["source"]["toolId"], "vscode-copilot");
    assert_eq!(vscode_range["source"]["sessionRef"], "vscode-one");
    assert_eq!(vscode_range["operation"], "create_file");
    assert_eq!(vscode_range["category"], "ai_direct");
    assert_eq!(vscode_range["score"], 1.0);
    assert_eq!(vscode_range["primaryBasis"], "DIRECT_ACCEPTED_EDIT");
    assert_eq!(codex_range["sourceTool"], "codex");
    assert!(codex_range["supportingBasis"].is_array());
    assert!(codex_range["limitingBasis"].is_array());
    assert_eq!(codex_range["algorithmProfile"], "deterministic-v1");
    assert_eq!(codex_range["scoreProfile"], "score-v1");
    assert_eq!(
        codex_range["endLineExclusive"],
        codex_range["endLine"].as_u64().unwrap() + 1
    );

    let table = fixture.cli(&[
        "summary",
        "--repo",
        fixture.repo.to_str().unwrap(),
        "--range",
        &format!("{base}..{head}"),
        "--file",
        "tracked.txt",
        "--format",
        "table",
    ]);
    assert!(table.status.success());
    let table = String::from_utf8_lossy(&table.stdout);
    assert_eq!(
        table.lines().next().unwrap(),
        "commit\tfiles\teligible\tai_direct\tai_strong\tai_probable\tmixed\tunknown\texcluded\tdeterministic_pct\tlinked_pct\tcoverage\ttools"
    );
    assert!(table.contains("\tai_strong\t"));
    assert!(table.contains("primaryBasis\tsupportingBasis\tlimitingBasis\tsourceTool\toperation"));
    assert!(!table.contains("\ts\t"));
    fixture.drop_root();
}

#[test]
fn structured_codex_patch_is_direct_but_legacy_events_remain_strong() {
    let fixture = Fixture::new("structured-direct");
    let patch = format!(
        "*** Begin Patch\n*** Update File: {}\n@@\n base\n+calculate_total = left + right\n*** End Patch",
        fixture.repo.join("tracked.txt").display()
    );
    let pre = json!({
        "hook_event_name":"PreToolUse", "session_id":"structured-session",
        "cwd":fixture.repo, "tool_name":"apply_patch", "tool_use_id":"patch-1",
        "tool_input":{"input":patch}
    });
    fixture.hook("codex", &pre);
    fs::write(
        fixture.repo.join("tracked.txt"),
        "base\ncalculate_total = left + right\n",
    )
    .unwrap();
    let mut post = pre.clone();
    post["hook_event_name"] = json!("PostToolUse");
    fixture.hook("codex", &post);
    let event_path = fs::read_dir(fixture.store.join("repos"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path()
        .join("events")
        .join("structured-session.jsonl");
    let event_text = fs::read_to_string(&event_path).unwrap();
    let change = event_text
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .find(|event| event["event"] == "ai_change")
        .unwrap();
    assert_eq!(change["structured_edit"], true);
    assert_eq!(
        change["structured_added_lines"],
        json!(["calculate_total = left + right"])
    );
    let commit = fixture.commit("structured patch");
    fixture.derive(&commit);
    let report = commit_report(&fixture, &commit);
    assert_eq!(report["totals"]["ai_direct"], 1);
    assert_eq!(report["totals"]["ai_strong"], 0);

    fixture.drop_root();

    // Removing the additive marker simulates a stored pre-r16 event; it must use scan attribution.
    let legacy_fixture = Fixture::new("legacy-scan-event");
    let legacy_patch = format!(
        "*** Begin Patch\n*** Update File: {}\n@@\n base\n+calculate_total = left + right\n*** End Patch",
        legacy_fixture.repo.join("tracked.txt").display()
    );
    let legacy_pre = json!({"hook_event_name":"PreToolUse","session_id":"legacy-session",
        "cwd":legacy_fixture.repo,"tool_name":"apply_patch","tool_use_id":"legacy-patch",
        "tool_input":{"input":legacy_patch}});
    legacy_fixture.hook("codex", &legacy_pre);
    fs::write(
        legacy_fixture.repo.join("tracked.txt"),
        "base\ncalculate_total = left + right\n",
    )
    .unwrap();
    let mut legacy_post = legacy_pre.clone();
    legacy_post["hook_event_name"] = json!("PostToolUse");
    legacy_fixture.hook("codex", &legacy_post);
    let legacy_event_path = fs::read_dir(legacy_fixture.store.join("repos"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path()
        .join("events")
        .join("legacy-session.jsonl");
    let legacy_text = fs::read_to_string(&legacy_event_path).unwrap();
    let mut legacy_lines = legacy_text
        .lines()
        .map(|line| {
            let mut value: Value = serde_json::from_str(line).unwrap();
            value.as_object_mut().unwrap().remove("structured_edit");
            value
                .as_object_mut()
                .unwrap()
                .remove("structured_added_lines");
            value
                .as_object_mut()
                .unwrap()
                .remove("structured_input_sha256");
            serde_json::to_string(&value).unwrap()
        })
        .collect::<Vec<_>>();
    legacy_lines.push(String::new());
    fs::write(&legacy_event_path, legacy_lines.join("\n")).unwrap();
    let legacy_commit = legacy_fixture.commit("legacy event replay");
    legacy_fixture.derive(&legacy_commit);
    let legacy = commit_report(&legacy_fixture, &legacy_commit);
    assert_eq!(legacy["totals"]["ai_direct"], 0);
    assert_eq!(legacy["totals"]["ai_strong"], 1);
    legacy_fixture.drop_root();
}

#[test]
fn direct_line_human_edit_becomes_mixed_and_overlap_is_capped() {
    let human = Fixture::new("direct-human-edit");
    let patch = format!(
        "*** Begin Patch\n*** Update File: {}\n@@\n base\n+calculate_total = left + right\n*** End Patch",
        human.repo.join("tracked.txt").display()
    );
    let pre = json!({"hook_event_name":"PreToolUse","session_id":"s","cwd":human.repo,
        "tool_name":"apply_patch","tool_use_id":"p","tool_input":{"input":patch}});
    human.hook("codex", &pre);
    fs::write(
        human.repo.join("tracked.txt"),
        "base\ncalculate_total = left + right\n",
    )
    .unwrap();
    let mut post = pre.clone();
    post["hook_event_name"] = json!("PostToolUse");
    human.hook("codex", &post);
    fs::write(
        human.repo.join("tracked.txt"),
        "base\ncalculate_total = left + right + offset\n",
    )
    .unwrap();
    let commit = human.commit("human follow-up");
    human.derive(&commit);
    let report = commit_report(&human, &commit);
    assert_eq!(report["totals"]["mixed"], 1);
    human.drop_root();

    let overlap = Fixture::new("direct-overlap");
    let patch = format!(
        "*** Begin Patch\n*** Update File: {}\n@@\n base\n+overlap line\n*** End Patch",
        overlap.repo.join("tracked.txt").display()
    );
    let first = json!({"hook_event_name":"PreToolUse","session_id":"first","cwd":overlap.repo,
        "tool_name":"apply_patch","tool_use_id":"first","tool_input":{"input":patch}});
    overlap.hook("codex", &first);
    let second = json!({"hook_event_name":"PreToolUse","session_id":"second","cwd":overlap.repo,
        "tool_name":"apply_patch","tool_use_id":"second","tool_input":{"input":patch}});
    overlap.hook("codex", &second);
    fs::write(overlap.repo.join("tracked.txt"), "base\noverlap line\n").unwrap();
    let mut second_post = second.clone();
    second_post["hook_event_name"] = json!("PostToolUse");
    overlap.hook("codex", &second_post);
    let mut first_post = first.clone();
    first_post["hook_event_name"] = json!("PostToolUse");
    overlap.hook("codex", &first_post);
    let commit = overlap.commit("overlapping edit");
    overlap.derive(&commit);
    let report = commit_report(&overlap, &commit);
    assert_eq!(report["totals"]["ai_direct"], 0);
    assert_eq!(report["totals"]["ai_strong"], 0);
    assert_eq!(report["totals"]["ai_probable"], 1);
    overlap.drop_root();
}

#[test]
fn summary_marks_missing_reports_and_zero_eligible_percentages() {
    let fixture = Fixture::new("summary-empty");
    fs::write(fixture.repo.join("blank.txt"), "\n  \n").unwrap();
    let blank_commit = fixture.commit("blank additions");
    fixture.derive(&blank_commit);
    let summary = fixture.cli(&[
        "summary",
        "--repo",
        fixture.repo.to_str().unwrap(),
        "--commit",
        &blank_commit,
        "--format",
        "json",
    ]);
    let summary: Value = parsed(&summary);
    assert_eq!(summary["commits"][0]["eligible"], 0);
    assert!(summary["commits"][0]["deterministic_pct"].is_null());
    assert!(summary["commits"][0]["linked_pct"].is_null());
    assert!(summary["totals"]["deterministic_pct"].is_null());
    assert!(summary["totals"]["linked_pct"].is_null());
    let table = fixture.cli(&[
        "summary",
        "--repo",
        fixture.repo.to_str().unwrap(),
        "--commit",
        &blank_commit,
        "--format",
        "table",
    ]);
    assert!(table.status.success());
    assert!(String::from_utf8_lossy(&table.stdout).contains("\tn/a\tn/a\tnone"));

    fs::write(fixture.repo.join("tracked.txt"), "base\nnot derived\n").unwrap();
    let missing_commit = fixture.commit("not derived");
    let latest = fixture.cli(&[
        "summary",
        "--repo",
        fixture.repo.to_str().unwrap(),
        "--last",
        "1",
        "--format",
        "json",
    ]);
    let latest: Value = parsed(&latest);
    assert_eq!(latest["commits"].as_array().unwrap().len(), 1);
    assert_eq!(latest["commits"][0]["commit"], blank_commit);
    let summary = fixture.cli(&[
        "summary",
        "--repo",
        fixture.repo.to_str().unwrap(),
        "--commit",
        &missing_commit,
        "--format",
        "json",
    ]);
    let summary: Value = parsed(&summary);
    assert_eq!(summary["commits"][0]["status"], "not derived");
    assert!(summary["commits"][0]["eligible"].is_null());
    assert_eq!(summary["totals"]["status"], "not derived");
    fixture.drop_root();
}
