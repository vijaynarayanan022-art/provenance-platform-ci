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
}

impl Fixture {
    fn new(name: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "prov-pr-report-{name}-{}-{nonce}",
            std::process::id()
        ));
        let repo = root.join("repository with spaces");
        let store = root.join("provenance store");
        fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init"]);
        git(&repo, &["config", "user.name", "PR Report Test"]);
        git(
            &repo,
            &["config", "user.email", "pr-report@example.invalid"],
        );
        fs::write(repo.join("tracked.txt"), "base\n").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-m", "initial"]);
        let fixture = Self { root, repo, store };
        fixture.hook("Stop", "seed-store");
        fixture
    }

    fn cli(&self, args: &[&str]) -> Output {
        isolated_command(env!("CARGO_BIN_EXE_prov-hook"))
            .args(args)
            .env("PROV_HOME", &self.store)
            .output()
            .unwrap()
    }

    fn base(&self) -> String {
        git_text(&self.repo, &["rev-parse", "HEAD"])
    }

    fn commit(&self, message: &str) -> String {
        git(&self.repo, &["add", "-A"]);
        git(&self.repo, &["commit", "-m", message]);
        git_text(&self.repo, &["rev-parse", "HEAD"])
    }

    fn ai_commit(&self, content: &str, message: &str) -> String {
        let tool = format!("tool-{}", message.replace(' ', "-"));
        self.hook("PreToolUse", &tool);
        fs::write(self.repo.join("tracked.txt"), content).unwrap();
        self.hook("PostToolUse", &tool);
        self.commit(message)
    }

    fn hook(&self, event: &str, tool_id: &str) {
        let payload = json!({
            "hook_event_name": event,
            "session_id": "pr-report-session",
            "turn_id": "pr-report-turn",
            "cwd": self.repo,
            "tool_use_id": tool_id,
            "tool_name": "PowerShell",
            "tool_input": {"command": "fixture command"},
        });
        let mut child = isolated_command(env!("CARGO_BIN_EXE_prov-hook"))
            .arg("codex")
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
            .write_all(payload.to_string().as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success());
        assert!(output.stdout.is_empty() && output.stderr.is_empty());
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

    fn report_file(&self, commit: &str) -> Value {
        let directory = fs::read_dir(self.store.join("repos")).unwrap();
        let repo_id = directory
            .into_iter()
            .next()
            .unwrap()
            .unwrap()
            .file_name()
            .to_string_lossy()
            .into_owned();
        serde_json::from_slice(
            &fs::read(
                self.store
                    .join("repos")
                    .join(repo_id)
                    .join("commits")
                    .join(format!("{commit}.json")),
            )
            .unwrap(),
        )
        .unwrap()
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
        .env("GIT_AUTHOR_NAME", "PR Report Test")
        .env("GIT_AUTHOR_EMAIL", "pr-report@example.invalid")
        .env("GIT_COMMITTER_NAME", "PR Report Test")
        .env("GIT_COMMITTER_EMAIL", "pr-report@example.invalid")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(output.success(), "git {args:?} failed for {:?}", repo);
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
    assert_eq!(output.stderr, b"");
    serde_json::from_slice(&output.stdout).expect("stdout must be pure JSON")
}

fn assert_keys(value: &Value, expected: &[&str]) {
    let mut actual: Vec<_> = value
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    let mut expected = expected.to_vec();
    actual.sort_unstable();
    expected.sort_unstable();
    assert_eq!(actual, expected);
}

#[test]
fn two_commit_report_sums_stored_reports_and_out_matches_stdout() {
    let fixture = Fixture::new("two-commit");
    let base = fixture.base();
    let first = fixture.ai_commit("base\nai first\n", "first AI change");
    fixture.derive(&first);
    let second = fixture.ai_commit("base\nai first\nai second\n", "second AI change");
    fixture.derive(&second);

    let args = [
        "pr-report",
        "--repo",
        fixture.repo.to_str().unwrap(),
        "--base",
        &base,
        "--head",
        &second,
        "--pr",
        "42",
        "--title",
        "Attribution report",
        "--author",
        "Copilot",
        "--status",
        "merged",
        "--url",
        "https://example.invalid/pull/42",
    ];
    let output = fixture.cli(&args);
    let value = parsed(&output);
    let first_report = fixture.report_file(&first);
    let second_report = fixture.report_file(&second);
    let expected_e = first_report["totals"]["e"].as_u64().unwrap()
        + second_report["totals"]["e"].as_u64().unwrap();
    let expected_strong = first_report["totals"]["ai_strong"].as_u64().unwrap()
        + second_report["totals"]["ai_strong"].as_u64().unwrap();
    let expected_probable = first_report["totals"]["ai_probable"].as_u64().unwrap()
        + second_report["totals"]["ai_probable"].as_u64().unwrap();
    assert_eq!(value["schema"], "prov.pr-report/0.1");
    assert_keys(
        &value,
        &[
            "schema",
            "created_at",
            "versions",
            "pr",
            "commits",
            "files_changed",
            "lines",
            "ai_deterministic_pct",
            "ai_linked_pct",
            "evidence",
            "coverage",
            "tools",
            "carried_lines",
            "top_files",
        ],
    );
    assert_keys(
        &value["pr"],
        &[
            "number",
            "title",
            "author",
            "status",
            "url",
            "base",
            "merge_base",
            "head",
        ],
    );
    assert_keys(
        &value["commits"],
        &["total", "derived", "underived", "truncated"],
    );
    assert_keys(
        &value["lines"],
        &[
            "eligible",
            "ai_direct",
            "ai_strong",
            "ai_probable",
            "mixed",
            "unknown",
            "excluded",
        ],
    );
    assert_keys(
        &value["evidence"],
        &["exact", "time_cut", "reconstructed", "none"],
    );
    assert_eq!(value["pr"]["number"], 42);
    assert_eq!(value["pr"]["base"], base);
    assert_eq!(value["pr"]["merge_base"], base);
    assert_eq!(value["pr"]["head"], second);
    assert_eq!(value["commits"]["total"], 2);
    assert_eq!(value["commits"]["derived"], 2);
    assert_eq!(value["lines"]["eligible"], expected_e);
    assert_eq!(value["lines"]["ai_strong"], expected_strong);
    assert_eq!(value["lines"]["ai_probable"], expected_probable);
    assert_eq!(
        value["ai_deterministic_pct"].as_f64().unwrap(),
        ((expected_strong as f64 * 1000.0 / expected_e as f64).round()) / 10.0
    );
    assert_eq!(value["ai_linked_pct"].as_f64().unwrap(), 100.0);
    assert_eq!(value["tools"], json!(["codex"]));
    assert!(!String::from_utf8_lossy(&output.stdout).contains("\"human\""));

    let output_path = fixture.root.join("report.json");
    let mut with_out = args.to_vec();
    with_out.extend(["--out", output_path.to_str().unwrap()]);
    let file_output = fixture.cli(&with_out);
    assert!(file_output.status.success());
    assert!(file_output.stdout.is_empty() && file_output.stderr.is_empty());
    let mut stdout_value: Value = serde_json::from_slice(&output.stdout).unwrap();
    let mut file_value: Value = serde_json::from_slice(&fs::read(output_path).unwrap()).unwrap();
    assert!(stdout_value["created_at"].as_str().unwrap().ends_with('Z'));
    assert!(file_value["created_at"].as_str().unwrap().ends_with('Z'));
    stdout_value["created_at"] = json!("normalized");
    file_value["created_at"] = json!("normalized");
    assert_eq!(file_value, stdout_value);
    fixture.drop_root();
}

#[test]
fn no_derive_reports_missing_commit_as_underived_without_failure() {
    let fixture = Fixture::new("underived");
    let base = fixture.base();
    fs::write(fixture.repo.join("tracked.txt"), "base\nnot yet derived\n").unwrap();
    let head = fixture.commit("underived change");
    let output = fixture.cli(&[
        "pr-report",
        "--repo",
        fixture.repo.to_str().unwrap(),
        "--base",
        &base,
        "--head",
        &head,
        "--no-derive",
    ]);
    let value = parsed(&output);
    assert_eq!(value["commits"]["total"], 1);
    assert_eq!(value["commits"]["derived"], 0);
    assert_eq!(value["commits"]["underived"], 1);
    assert!(value["lines"]["eligible"].is_null());
    assert!(value["coverage"]["band"].is_null());
    fixture.drop_root();
}

#[test]
fn never_captured_commit_derives_unknown_lines_not_ai() {
    let fixture = Fixture::new("unknown-lines");
    let base = fixture.base();
    fs::write(fixture.repo.join("tracked.txt"), "base\nuncaptured line\n").unwrap();
    let head = fixture.commit("uncaptured change");
    let output = fixture.cli(&[
        "pr-report",
        "--repo",
        fixture.repo.to_str().unwrap(),
        "--base",
        &base,
        "--head",
        &head,
    ]);
    let value = parsed(&output);
    assert_eq!(value["commits"]["derived"], 1);
    assert_eq!(value["lines"]["eligible"], 1);
    assert_eq!(value["lines"]["unknown"], 1);
    assert_eq!(value["lines"]["ai_strong"], 0);
    assert_eq!(value["lines"]["ai_probable"], 0);
    fixture.drop_root();
}

#[test]
fn prs_file_produces_dashboard_in_input_order() {
    let fixture = Fixture::new("dashboard");
    let base = fixture.base();
    let head = fixture.ai_commit("base\none line\n", "one PR change");
    fixture.derive(&head);
    let input = json!([
        {"number":30,"title":"third","author":"c","status":"closed","url":"u3","base":base,"head":head},
        {"number":10,"title":"first","author":"a","status":"open","url":"u1","base":base,"head":head},
        {"number":20,"title":"second","author":"b","status":"merged","url":"u2","base":base,"head":head}
    ]);
    let prs_path = fixture.root.join("prs.json");
    fs::write(&prs_path, serde_json::to_vec(&input).unwrap()).unwrap();
    let output = fixture.cli(&[
        "pr-report",
        "--repo",
        fixture.repo.to_str().unwrap(),
        "--prs-file",
        prs_path.to_str().unwrap(),
    ]);
    let value = parsed(&output);
    assert_eq!(value["schema"], "prov.dashboard/0.1");
    assert!(value["generated_at"].as_str().unwrap().ends_with('Z'));
    assert_eq!(value["prs"].as_array().unwrap().len(), 3);
    let pr = &value["prs"][0];
    assert!(pr["created_at"].as_str().unwrap().ends_with('Z'));
    assert_eq!(
        pr["versions"],
        json!({
            "engine": env!("CARGO_PKG_VERSION"),
            "algorithm": "derive-v1",
            "line_survival": "unobserved-mixed-v1",
            "min_mixed_overlap": 0.5,
            "formula": "strict-v1",
            "eligibility": "non-blank-added-v1",
            "exclusion_classifier": "exclusion-v1",
            "coverage": "coverage-v1",
        })
    );
    assert_eq!(
        pr["coverage"],
        json!({
            "band": "high",
            "analysis": "complete",
            "content": "complete",
            "source": "high",
            "history": "unknown",
        })
    );
    assert_eq!(value["prs"][0]["pr"]["number"], 30);
    assert_eq!(value["prs"][1]["pr"]["number"], 10);
    assert_eq!(value["prs"][2]["pr"]["number"], 20);
    fixture.drop_root();
}

#[test]
fn amended_pr_reports_carried_lines_and_keeps_carried_labels() {
    let fixture = Fixture::new("amended");
    let base = fixture.base();
    let phrase = "AI authored carried phrase";
    let source = fixture.ai_commit(&format!("base\n{phrase}\n"), "AI source");
    fixture.derive(&source);
    git(
        &fixture.repo,
        &["commit", "--amend", "-m", "amended source"],
    );
    let head = git_text(&fixture.repo, &["rev-parse", "HEAD"]);
    let output = fixture.cli(&[
        "pr-report",
        "--repo",
        fixture.repo.to_str().unwrap(),
        "--base",
        &base,
        "--head",
        &head,
    ]);
    let value = parsed(&output);
    assert_eq!(value["commits"]["derived"], 1);
    assert!(value["carried_lines"].as_u64().unwrap() > 0);
    assert_eq!(value["lines"]["ai_strong"], 1);
    assert_eq!(value["tools"], json!(["codex"]));
    fixture.drop_root();
}

#[test]
fn merge_commits_are_skipped_and_empty_range_has_null_percentages() {
    let fixture = Fixture::new("merge-empty");
    let base = fixture.base();
    git(&fixture.repo, &["checkout", "-b", "feature"]);
    fs::write(fixture.repo.join("feature.txt"), "feature\n").unwrap();
    let _feature = fixture.commit("feature change");
    git(&fixture.repo, &["checkout", "-"]);
    fs::write(fixture.repo.join("main.txt"), "main\n").unwrap();
    let _main = fixture.commit("main change");
    git(
        &fixture.repo,
        &["merge", "--no-ff", "feature", "-m", "merge feature"],
    );
    let merge_head = git_text(&fixture.repo, &["rev-parse", "HEAD"]);

    let merged = fixture.cli(&[
        "pr-report",
        "--repo",
        fixture.repo.to_str().unwrap(),
        "--base",
        &base,
        "--head",
        &merge_head,
    ]);
    let merged_value = parsed(&merged);
    assert_eq!(merged_value["commits"]["total"], 2);
    assert_eq!(merged_value["commits"]["derived"], 2);

    let empty = fixture.cli(&[
        "pr-report",
        "--repo",
        fixture.repo.to_str().unwrap(),
        "--base",
        &merge_head,
        "--head",
        &merge_head,
    ]);
    let empty_value = parsed(&empty);
    assert_eq!(empty_value["commits"]["total"], 0);
    assert_eq!(empty_value["lines"]["eligible"], 0);
    assert!(empty_value["ai_deterministic_pct"].is_null());
    assert!(empty_value["ai_linked_pct"].is_null());
    assert!(!String::from_utf8_lossy(&empty.stdout).contains("\"human\""));
    fixture.drop_root();
}

#[test]
fn deleted_path_counts_as_changed_without_inventing_eligible_lines() {
    let fixture = Fixture::new("deleted-path");
    let base = fixture.base();
    fs::remove_file(fixture.repo.join("tracked.txt")).unwrap();
    let head = fixture.commit("delete tracked file");
    fixture.derive(&head);
    let output = fixture.cli(&[
        "pr-report",
        "--repo",
        fixture.repo.to_str().unwrap(),
        "--base",
        &base,
        "--head",
        &head,
    ]);
    let value = parsed(&output);
    assert_eq!(value["files_changed"], 1);
    assert_eq!(value["lines"]["eligible"], 0);
    assert!(value["ai_deterministic_pct"].is_null());
    assert!(value["ai_linked_pct"].is_null());
    fixture.drop_root();
}

#[test]
fn advanced_base_changes_do_not_inflate_pr_files_changed() {
    let fixture = Fixture::new("merge-base-advanced-base");
    let fork_point = fixture.base();
    let base_branch = git_text(&fixture.repo, &["symbolic-ref", "--short", "HEAD"]);
    git(&fixture.repo, &["checkout", "-b", "pr-branch"]);
    for index in 0..5 {
        fs::write(
            fixture.repo.join(format!("pr-file-{index}.txt")),
            format!("PR change {index}\n"),
        )
        .unwrap();
    }
    let pr_head = fixture.commit("change five PR files");

    git(&fixture.repo, &["checkout", &base_branch]);
    fs::write(fixture.repo.join("notes.txt"), "base-only notes change\n").unwrap();
    let advanced_base = fixture.commit("advance base with notes");

    let output = fixture.cli(&[
        "pr-report",
        "--repo",
        fixture.repo.to_str().unwrap(),
        "--base",
        &advanced_base,
        "--head",
        &pr_head,
        "--no-derive",
    ]);
    let value = parsed(&output);
    assert_eq!(value["pr"]["base"], advanced_base);
    assert_eq!(value["pr"]["merge_base"], fork_point);
    assert_eq!(value["pr"]["head"], pr_head);
    assert_eq!(value["files_changed"], 5);
    assert_eq!(value["commits"]["total"], 1);
    fixture.drop_root();
}

#[test]
fn unrelated_histories_report_null_merge_base_and_changed_files() {
    let fixture = Fixture::new("merge-base-unrelated");
    let base = fixture.base();
    git(
        &fixture.repo,
        &["checkout", "--orphan", "unrelated-history"],
    );
    fs::remove_file(fixture.repo.join("tracked.txt")).unwrap();
    fs::write(fixture.repo.join("unrelated.txt"), "independent root\n").unwrap();
    let head = fixture.commit("unrelated root commit");

    let output = fixture.cli(&[
        "pr-report",
        "--repo",
        fixture.repo.to_str().unwrap(),
        "--base",
        &base,
        "--head",
        &head,
        "--no-derive",
    ]);
    let value = parsed(&output);
    assert_eq!(value["schema"], "prov.pr-report/0.1");
    assert_eq!(value["pr"]["base"], base);
    assert_eq!(value["pr"]["head"], head);
    assert!(value["pr"]["merge_base"].is_null());
    assert!(value["files_changed"].is_null());
    assert_eq!(value["commits"]["total"], 1);
    fixture.drop_root();
}
