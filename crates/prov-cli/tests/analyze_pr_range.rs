//! Integration tests for `prov analyze --base REV --head REV` (PR range mode).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Unique per call. The clock alone is not enough: on Windows its resolution is coarse, so two
/// parallel tests can get the same timestamp and `git init` the same folder at once.
static COUNTER: AtomicUsize = AtomicUsize::new(0);

fn git(dir: &Path, empty_cfg: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", empty_cfg)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {:?}: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn prov(repo: &Path, cfg: &Path, extra: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_prov"))
        .arg("analyze")
        .arg("--repo")
        .arg(repo)
        .args(extra)
        .env("GIT_CONFIG_GLOBAL", cfg)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap()
}

fn setup_repo() -> (PathBuf, PathBuf) {
    let id = format!(
        "{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        COUNTER.fetch_add(1, Ordering::SeqCst)
    );
    let dir = std::env::temp_dir().join(format!("prov-pr-test-{id}"));
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = dir.join("empty.gitconfig");
    std::fs::write(&cfg, "").unwrap();
    let repo = dir.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &cfg, &["init", "-q", "-b", "main"]);
    git(&repo, &cfg, &["config", "core.autocrlf", "false"]);
    (repo, cfg)
}

fn write_note(repo: &Path, cfg: &Path, commit: &str, note_content: &str) {
    let note_path = repo.join(".git").join("temp_note.txt");
    std::fs::write(&note_path, note_content).unwrap();
    git(
        repo,
        cfg,
        &[
            "notes",
            "--ref=ai",
            "add",
            "-f",
            "-F",
            note_path.to_str().unwrap(),
            commit,
        ],
    );
    let _ = std::fs::remove_file(note_path);
}

fn write_checkpoints(repo: &Path, parent_sha: &str, content: &str) {
    let wl = repo
        .join(".git/ai/working_logs")
        .join(format!("old-{parent_sha}"));
    std::fs::create_dir_all(&wl).unwrap();
    std::fs::write(wl.join("checkpoints.jsonl"), content).unwrap();
}

fn setup_ai_pr(note_content: &str) -> (PathBuf, PathBuf, String, String) {
    let (repo, cfg) = setup_repo();
    std::fs::write(repo.join("base.txt"), "base\n").unwrap();
    git(&repo, &cfg, &["add", "base.txt"]);
    git(&repo, &cfg, &["commit", "-q", "-m", "base"]);
    let base = git(&repo, &cfg, &["rev-parse", "HEAD"]);
    std::fs::write(repo.join("g.txt"), "line one\nline two\n").unwrap();
    git(&repo, &cfg, &["add", "g.txt"]);
    git(&repo, &cfg, &["commit", "-q", "-m", "feature"]);
    let head = git(&repo, &cfg, &["rev-parse", "HEAD"]);
    write_note(&repo, &cfg, &head, note_content);
    (repo, cfg, base, head)
}

fn setup_linecheck_pr(content: &str, note_content: &str) -> (PathBuf, PathBuf, String, String) {
    let (repo, cfg) = setup_repo();
    std::fs::write(repo.join("base.txt"), "base\n").unwrap();
    git(&repo, &cfg, &["add", "base.txt"]);
    git(&repo, &cfg, &["commit", "-q", "-m", "base"]);
    let base = git(&repo, &cfg, &["rev-parse", "HEAD"]);
    std::fs::write(repo.join("g.txt"), content).unwrap();
    git(&repo, &cfg, &["add", "g.txt"]);
    git(&repo, &cfg, &["commit", "-q", "-m", "feature"]);
    let head = git(&repo, &cfg, &["rev-parse", "HEAD"]);
    write_note(&repo, &cfg, &head, note_content);
    (repo, cfg, base, head)
}

fn setup_multicommit_linecheck_pr(
    committed_at_a: &str,
    blob: &str,
    note_content: &str,
) -> (PathBuf, PathBuf, String, String, String) {
    let (repo, cfg) = setup_repo();
    std::fs::write(repo.join("base.txt"), "base\n").unwrap();
    git(&repo, &cfg, &["add", "base.txt"]);
    git(&repo, &cfg, &["commit", "-q", "-m", "base"]);
    let base = git(&repo, &cfg, &["rev-parse", "HEAD"]);

    std::fs::write(repo.join("g.txt"), committed_at_a).unwrap();
    git(&repo, &cfg, &["add", "g.txt"]);
    git(&repo, &cfg, &["commit", "-q", "-m", "AI feature"]);
    let commit_a = git(&repo, &cfg, &["rev-parse", "HEAD"]);
    write_note(&repo, &cfg, &commit_a, note_content);
    append_blob_checkpoint(&repo, &base, "t_a", "g.txt", blob);

    std::fs::write(repo.join("g.txt"), format!("top one\ntop two\n{committed_at_a}"))
        .unwrap();
    git(&repo, &cfg, &["add", "g.txt"]);
    git(&repo, &cfg, &["commit", "-q", "-m", "prepend uncredited lines"]);
    let commit_b = git(&repo, &cfg, &["rev-parse", "HEAD"]);
    (repo, cfg, base, commit_a, commit_b)
}

fn append_blob_checkpoint(
    repo: &Path,
    parent: &str,
    trace: &str,
    file: &str,
    blob: &str,
) {
    let folder = repo.join(".git/ai/working_logs").join(format!("old-{parent}"));
    let blobs = folder.join("blobs");
    std::fs::create_dir_all(&blobs).unwrap();
    let blob_sha = prov_gitai::sha256_hex(blob.as_bytes());
    std::fs::write(blobs.join(&blob_sha), blob).unwrap();
    let checkpoint = serde_json::json!({
        "kind": "AiAgent",
        "trace_id": trace,
        "timestamp": 123,
        "entries": [{ "file": file, "blob_sha": blob_sha }]
    });
    let path = folder.join("checkpoints.jsonl");
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new().create(true).append(true).open(path).unwrap();
    writeln!(file, "{checkpoint}").unwrap();
}

#[test]
fn test_multicommit_pr_analysis_and_aggregation() {
    let (repo, cfg) = setup_repo();

    // Base commit
    std::fs::write(repo.join("file.txt"), "base 1\nbase 2\n").unwrap();
    git(&repo, &cfg, &["add", "file.txt"]);
    git(&repo, &cfg, &["commit", "-q", "-m", "base"]);
    let base = git(&repo, &cfg, &["rev-parse", "HEAD"]);

    // Commit 1: 4 lines added by AI
    std::fs::write(
        repo.join("file.txt"),
        "base 1\nbase 2\nc1_line1\nc1_line2\nc1_line3\nc1_line4\n",
    )
    .unwrap();
    git(&repo, &cfg, &["add", "file.txt"]);
    git(&repo, &cfg, &["commit", "-q", "-m", "feat commit 1"]);
    let c1 = git(&repo, &cfg, &["rev-parse", "HEAD"]);

    let c1_note = "\
file.txt
  s_1::t_c1 3-6
---
{\"schema_version\":\"authorship/3.0.0\"}
";
    write_note(&repo, &cfg, &c1, c1_note);
    write_checkpoints(
        &repo,
        &base,
        "{\"kind\":\"AiAgent\",\"trace_id\":\"t_c1\"}\n",
    );

    // Commit 2: 2 lines human (7-8), 2 lines AI (9-10)
    std::fs::write(
        repo.join("file.txt"),
        "base 1\nbase 2\nc1_line1\nc1_line2\nc1_line3\nc1_line4\nhuman_7\nhuman_8\nc2_line9\nc2_line10\n",
    )
    .unwrap();
    git(&repo, &cfg, &["add", "file.txt"]);
    git(&repo, &cfg, &["commit", "-q", "-m", "feat commit 2"]);
    let c2 = git(&repo, &cfg, &["rev-parse", "HEAD"]);

    let c2_note = "\
file.txt
  s_2::t_c2 9-10
---
{\"schema_version\":\"authorship/3.0.0\"}
";
    write_note(&repo, &cfg, &c2, c2_note);
    write_checkpoints(
        &repo,
        &c1,
        "{\"kind\":\"AiAgent\",\"trace_id\":\"t_c2\"}\n",
    );

    // Analyze PR base..c2
    let out = prov(&repo, &cfg, &["--base", &base, "--head", &c2, "--json"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();

    assert_eq!(v["mode"], "pr_range");
    assert_eq!(v["schema_version"], "prov.analysis/0.2");
    assert_eq!(v["commits_in_range"], 2);
    assert_eq!(v["eligible_added_lines"], 8);
    assert_eq!(v["counts"]["strong"], 6); // 4 from c1 + 2 from c2
    assert_eq!(v["counts"]["unknown"], 2); // 2 human lines from c2
    let det = v["ai_deterministic_pct"].as_f64().unwrap();
    assert!((det - 75.0).abs() < 1e-3, "expected 75.0, got {det}");
    let linked = v["ai_linked_pct"].as_f64().unwrap();
    assert!((linked - 75.0).abs() < 1e-3, "expected 75.0, got {linked}");

    let per_commit = v["per_commit"].as_array().unwrap();
    assert_eq!(per_commit.len(), 2);
    assert_eq!(per_commit[0]["sha"], c1);
    assert_eq!(per_commit[0]["lines_in_final"], 4);
    assert_eq!(per_commit[0]["note_found"], true);
    assert_eq!(per_commit[0]["checkpoints_found"], true);

    assert_eq!(per_commit[1]["sha"], c2);
    assert_eq!(per_commit[1]["lines_in_final"], 4);
    assert_eq!(per_commit[1]["note_found"], true);
    assert_eq!(per_commit[1]["checkpoints_found"], true);
}

#[test]
fn test_missing_checkpoints_make_ai_claims_probable_and_warn() {
    let (repo, cfg) = setup_repo();
    std::fs::write(repo.join("base.txt"), "base\n").unwrap();
    git(&repo, &cfg, &["add", "base.txt"]);
    git(&repo, &cfg, &["commit", "-q", "-m", "base"]);
    let base = git(&repo, &cfg, &["rev-parse", "HEAD"]);
    std::fs::write(repo.join("g.txt"), "ai one\nai two\n").unwrap();
    git(&repo, &cfg, &["add", "g.txt"]);
    git(&repo, &cfg, &["commit", "-q", "-m", "feature"]);
    let head = git(&repo, &cfg, &["rev-parse", "HEAD"]);
    write_note(&repo, &cfg, &head, "g.txt\n  s_1::t_ai 1-2\n---\n{\"schema_version\":\"authorship/3.0.0\"}\n");

    let out = prov(&repo, &cfg, &["--base", &base, "--head", &head, "--json"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["counts"]["probable"], 2);
    assert_eq!(v["counts"]["strong"], 0);
    assert_eq!(v["unattributed_lines"], 0);
    assert_eq!(v["per_commit"][0]["checkpoints_found"], false);
    assert!(v["warnings"].to_string().contains("could NOT be verified"));
}

#[test]
fn test_pr_none_recorded_checkpoints_make_ai_trace_unbacked() {
    let (repo, cfg, base, head) = setup_ai_pr(
        "g.txt\n  s_1::t_ai 1-2\n---\n{\"schema_version\":\"authorship/3.0.0\"}\n",
    );
    let folder = repo.join(".git/ai/working_logs").join(format!("old-{base}"));
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(folder.join(".archived_at"), "archived").unwrap();

    let out = prov(&repo, &cfg, &["--base", &base, "--head", &head, "--json"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["per_commit"][0]["checkpoints_state"], "none_recorded");
    assert_eq!(v["counts"]["unknown"], 2);
    assert_eq!(v["unbacked_ai_lines"], 2);
    assert!(v["warnings"].to_string().contains(
        "Git AI recorded no raw checkpoints for this commit but the note claims AI; 2 line(s) counted as unknown"
    ));
    assert!(!v["warnings"].to_string().contains("could NOT be verified"));
}

#[test]
fn test_pr_none_recorded_does_not_warn_when_ai_claim_matches_no_added_lines() {
    let (repo, cfg, base, head) = setup_ai_pr(
        "g.txt\n  s_1::t_ai 100-101\n---\n{\"schema_version\":\"authorship/3.0.0\"}\n",
    );
    let folder = repo.join(".git/ai/working_logs").join(format!("old-{base}"));
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(folder.join(".archived_at"), "archived").unwrap();

    let out = prov(&repo, &cfg, &["--base", &base, "--head", &head, "--json"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["per_commit"][0]["checkpoints_state"], "none_recorded");
    assert_eq!(v["unbacked_ai_lines"], 0);
    assert!(!v["warnings"].to_string().contains("Git AI recorded no raw checkpoints"));
}

#[test]
fn test_legacy_prompt_claim_with_none_recorded_checkpoints() {
    let (repo, cfg, base, head) = setup_ai_pr(
        "g.txt\n  abc123 1-2\n---\n{\"schema_version\":\"authorship/3.0.0\"}\n",
    );
    let folder = repo.join(".git/ai/working_logs").join(format!("old-{base}"));
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(folder.join(".archived_at"), "archived").unwrap();

    let out = prov(&repo, &cfg, &["--base", &base, "--head", &head, "--json", "--explain"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(value["per_commit"][0]["checkpoints_state"], "none_recorded");
    assert_eq!(value["counts"]["probable"], 2);
    assert_eq!(value["counts"]["unknown"], 0);
    assert_eq!(value["explanations"][0]["category"], "ai_probable");
    assert_eq!(value["explanations"][0]["reason_code"], "ai_unverified");
    assert!(!value["warnings"].to_string().contains("Git AI recorded no raw checkpoints"));
}

#[test]
fn test_pr_missing_checkpoints_state_is_missing() {
    let (repo, cfg, base, head) = setup_ai_pr(
        "g.txt\n  s_1::t_ai 1-2\n---\n{\"schema_version\":\"authorship/3.0.0\"}\n",
    );
    let out = prov(&repo, &cfg, &["--base", &base, "--head", &head, "--json"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["per_commit"][0]["checkpoints_state"], "missing");
    assert_eq!(v["counts"]["probable"], 2);
}

#[test]
fn test_pr_human_only_note_with_none_recorded_has_no_checkpoint_warning() {
    let (repo, cfg, base, head) = setup_ai_pr(
        "g.txt\n  h_editor 1-2\n---\n{\"schema_version\":\"authorship/3.0.0\"}\n",
    );
    let folder = repo.join(".git/ai/working_logs").join(format!("old-{base}"));
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(folder.join(".archived_at"), "archived").unwrap();

    let out = prov(&repo, &cfg, &["--base", &base, "--head", &head, "--json"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["per_commit"][0]["checkpoints_state"], "none_recorded");
    assert!(!v["warnings"].to_string().contains("raw checkpoints"));
}

#[test]
fn test_pr_editor_marker_output_uses_no_human_terminology() {
    let (repo, cfg, base, head) = setup_ai_pr(
        "g.txt\n  h_editor 1-2\n---\n{\"schema_version\":\"authorship/3.0.0\"}\n",
    );

    for args in [
        vec!["--base", &base, "--head", &head, "--explain"],
        vec!["--base", &base, "--head", &head, "--json", "--explain"],
    ] {
        let out = prov(&repo, &cfg, &args);
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let whole_output = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
        .to_lowercase();
        assert!(!whole_output.contains("human"), "{whole_output}");
        if args.contains(&"--json") {
            let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
            assert_eq!(value["editor_marked_lines"], 2);
            assert_eq!(value["explanations"][0]["reason_code"], "editor_marker");
        }
    }
}

#[test]
fn test_human_only_note_does_not_warn_about_missing_checkpoints_in_pr() {
    let (repo, cfg) = setup_repo();
    std::fs::write(repo.join("base.txt"), "base\n").unwrap();
    git(&repo, &cfg, &["add", "base.txt"]);
    git(&repo, &cfg, &["commit", "-q", "-m", "base"]);
    let base = git(&repo, &cfg, &["rev-parse", "HEAD"]);
    std::fs::write(repo.join("human.txt"), "human line\n").unwrap();
    git(&repo, &cfg, &["add", "human.txt"]);
    git(&repo, &cfg, &["commit", "-q", "-m", "human addition"]);
    let head = git(&repo, &cfg, &["rev-parse", "HEAD"]);
    write_note(&repo, &cfg, &head, "human.txt\n  h_human 1\n---\n{\"schema_version\":\"authorship/3.0.0\"}\n");

    let out = prov(&repo, &cfg, &["--base", &base, "--head", &head, "--json"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["counts"]["unknown"], 1);
    assert_eq!(v["editor_marked_lines"], 1);
    assert!(!v["warnings"].to_string().contains("raw checkpoints not found"));
    assert!(!v["warnings"].to_string().contains("could NOT be verified"));
}

#[test]
fn test_pr_counts_only_nonblank_added_lines() {
    let (repo, cfg) = setup_repo();
    std::fs::write(repo.join("base.txt"), "base\n").unwrap();
    git(&repo, &cfg, &["add", "base.txt"]);
    git(&repo, &cfg, &["commit", "-q", "-m", "base"]);
    let base = git(&repo, &cfg, &["rev-parse", "HEAD"]);

    std::fs::write(repo.join("mixed.txt"), "ai one\n\n \t \nai two\n").unwrap();
    std::fs::write(repo.join("blank.txt"), "\n \t\n").unwrap();
    git(&repo, &cfg, &["add", "mixed.txt", "blank.txt"]);
    git(&repo, &cfg, &["commit", "-q", "-m", "AI with blank lines"]);
    let head = git(&repo, &cfg, &["rev-parse", "HEAD"]);
    write_note(
        &repo,
        &cfg,
        &head,
        "mixed.txt\n  s_1::t_ai 1,4\n---\n{\"schema_version\":\"authorship/3.0.0\"}\n",
    );
    write_checkpoints(&repo, &base, "{\"kind\":\"AiAgent\",\"trace_id\":\"t_ai\"}\n");

    let json_out = prov(&repo, &cfg, &["--base", &base, "--head", &head, "--json"]);
    assert!(json_out.status.success(), "{}", String::from_utf8_lossy(&json_out.stderr));
    let v: serde_json::Value = serde_json::from_slice(&json_out.stdout).unwrap();
    assert_eq!(v["counting_policy"], "nonblank-v1");
    assert_eq!(v["blank_added_lines"], 4);
    assert_eq!(v["eligible_added_lines"], 2);
    assert_eq!(v["counts"]["strong"], 2);
    assert_eq!(v["counts"]["probable"], 0);
    assert_eq!(v["counts"]["excluded"], 0);

    let text_out = prov(&repo, &cfg, &["--base", &base, "--head", &head]);
    assert!(text_out.status.success(), "{}", String::from_utf8_lossy(&text_out.stderr));
    let report = String::from_utf8_lossy(&text_out.stdout);
    assert!(report.contains("Counting policy: nonblank-v1"));
    assert!(report.contains("Blank added lines: 4"));
}

#[test]
fn test_pr_linecheck_counts_replacement_blank_and_unbacked_lines() {
    let blob = "first\nAI second\nthird\n";
    let committed = "first\nHuman rewrite\nthird\n\nnew one\nnew two\n";
    let note = "g.txt\n  s_1::t_trace 1-3\n  s_2::t_later 5-6\n---\n{\"schema_version\":\"authorship/3.0.0\"}\n";
    let (repo, cfg, base, head) = setup_linecheck_pr(committed, note);
    append_blob_checkpoint(&repo, &base, "t_trace", "g.txt", blob);

    let out = prov(&repo, &cfg, &["--base", &base, "--head", &head, "--json", "--explain"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(value["per_commit"][0]["line_check"], "verified");
    assert_eq!(value["blank_added_lines"], 1);
    assert_eq!(value["counts"]["strong"], 2);
    assert_eq!(value["counts"]["mixed"], 1);
    assert_eq!(value["counts"]["unknown"], 2);
    assert_eq!(value["ai_deterministic_pct"], 40.0);
    assert_eq!(value["ai_linked_pct"], 60.0);
    assert_eq!(value["explanations"].as_array().unwrap().iter()
        .find(|line| line["line"] == 2).unwrap()["reason_code"], "ai_edited_after_checkpoint");

    let text = prov(&repo, &cfg, &["--base", &base, "--head", &head, "--explain"]);
    assert!(text.status.success(), "{}", String::from_utf8_lossy(&text.stderr));
    let report = String::from_utf8_lossy(&text.stdout);
    assert!(report.contains("L2: mixed"));
    assert!(report.contains("ai_edited_after_checkpoint"));
    assert!(report.contains("unbacked_trace"));
}

#[test]
fn test_linecheck_in_multicommit_pr_uses_commit_a_text_after_later_shift() {
    let blob = "first\nsecond\nthird\n";
    let note = "g.txt\n  s_1::t_a 1-3\n---\n{\"schema_version\":\"authorship/3.0.0\"}\n";
    let (repo, cfg, base, commit_a, commit_b) =
        setup_multicommit_linecheck_pr(blob, blob, note);
    let out = prov(
        &repo,
        &cfg,
        &["--base", &base, "--head", &commit_b, "--json", "--explain"],
    );
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(value["commits_in_range"], 2);
    assert_eq!(value["eligible_added_lines"], 5);
    assert_eq!(value["counts"]["strong"], 3);
    assert_eq!(value["counts"]["unknown"], 2);
    assert_eq!(value["counts"]["mixed"], 0);
    let commits = value["per_commit"].as_array().unwrap();
    let a = commits.iter().find(|commit| commit["sha"] == commit_a).unwrap();
    assert_eq!(a["line_check"], "verified");
    let explanations = value["explanations"].as_array().unwrap();
    for line in 3..=5 {
        assert_eq!(
            explanations.iter().find(|item| item["line"] == line).unwrap()["category"],
            "ai_strong"
        );
    }
    for line in 1..=2 {
        assert_eq!(
            explanations.iter().find(|item| item["line"] == line).unwrap()["category"],
            "unknown"
        );
    }
}

#[test]
fn test_linecheck_in_multicommit_pr_preserves_edit_after_later_shift() {
    let blob = "first\nAI second\nthird\n";
    let committed_at_a = "first\nHuman rewrite\nthird\n";
    let note = "g.txt\n  s_1::t_a 1-3\n---\n{\"schema_version\":\"authorship/3.0.0\"}\n";
    let (repo, cfg, base, commit_a, commit_b) =
        setup_multicommit_linecheck_pr(committed_at_a, blob, note);
    let out = prov(
        &repo,
        &cfg,
        &["--base", &base, "--head", &commit_b, "--json", "--explain"],
    );
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(value["eligible_added_lines"], 5);
    assert_eq!(value["counts"]["strong"], 2);
    assert_eq!(value["counts"]["mixed"], 1);
    assert_eq!(value["counts"]["unknown"], 2);
    let a = value["per_commit"]
        .as_array().unwrap()
        .iter().find(|commit| commit["sha"] == commit_a).unwrap();
    assert_eq!(a["line_check"], "verified");
    let explanations = value["explanations"].as_array().unwrap();
    assert_eq!(explanations.iter().find(|item| item["line"] == 4).unwrap()["category"], "mixed");
    assert_eq!(explanations.iter().find(|item| item["line"] == 4).unwrap()["reason_code"], "ai_edited_after_checkpoint");
    assert_eq!(explanations.iter().find(|item| item["line"] == 3).unwrap()["category"], "ai_strong");
    assert_eq!(explanations.iter().find(|item| item["line"] == 5).unwrap()["category"], "ai_strong");
}

#[test]
fn test_linecheck_preserves_indentation_and_ignores_trailing_blank_lines() {
    let content = "  first\nsecond\n\n\n";
    let note = "g.txt\n  s_1::t_trace 1-2\n---\n{\"schema_version\":\"authorship/3.0.0\"}\n";
    let (repo, cfg, base, head) = setup_linecheck_pr(content, note);
    append_blob_checkpoint(&repo, &base, "t_trace", "g.txt", content);
    let out = prov(&repo, &cfg, &["--base", &base, "--head", &head, "--json", "--explain"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(value["per_commit"][0]["line_check"], "verified");
    assert_eq!(value["blank_added_lines"], 2);
    assert_eq!(value["eligible_added_lines"], 2);
    assert_eq!(value["counts"]["strong"], 2);
    assert_eq!(value["counts"]["mixed"], 0);
    assert_eq!(value["counts"]["unknown"], 0);
}

#[test]
fn test_pr_file_rename_after_ai_commit_does_not_crash() {
    let blob = "first\nsecond\nthird\n";
    let note = "g.txt\n  s_1::t_a 1-3\n---\n{\"schema_version\":\"authorship/3.0.0\"}\n";
    let (repo, cfg, base, commit_a, _) = setup_multicommit_linecheck_pr(blob, blob, note);
    git(&repo, &cfg, &["mv", "g.txt", "h.txt"]);
    git(&repo, &cfg, &["commit", "-q", "-m", "rename AI file"]);
    let head = git(&repo, &cfg, &["rev-parse", "HEAD"]);
    let out = prov(&repo, &cfg, &["--base", &base, "--head", &head, "--json", "--explain"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(value["commits_in_range"], 3);
    assert_eq!(value["counts"]["mixed"], 0);
    let a = value["per_commit"].as_array().unwrap().iter()
        .find(|commit| commit["sha"] == commit_a).unwrap();
    // The note still names g.txt, while the range diff and blame use h.txt. The existing
    // path-based note matching therefore yields unknown lines and does not run line checking.
    assert_eq!(a["line_check"], "not_applicable");
    assert_eq!(value["counts"]["unknown"], 5);
    assert!(!value["warnings"].to_string().contains("line-level check unavailable"));
}

#[test]
fn test_linecheck_reason_codes_are_in_text_and_json_without_human_output() {
    let blob = "first\nsecond\nthird\nmarker\nlast\n";
    let committed = "first\nedited\nthird\ninserted\nmarker\nlast\n";
    let note = "g.txt\n  s_1::t_trace 1-6\n  h_editor 5\n---\n{\"schema_version\":\"authorship/3.0.0\"}\n";
    let (repo, cfg, base, head) = setup_linecheck_pr(committed, note);
    append_blob_checkpoint(&repo, &base, "t_trace", "g.txt", blob);

    for args in [
        vec!["--base", &base, "--head", &head, "--explain"],
        vec!["--base", &base, "--head", &head, "--json", "--explain"],
    ] {
        let out = prov(&repo, &cfg, &args);
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let whole_output = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ).to_lowercase();
        assert!(!whole_output.contains("human"), "{whole_output}");
        assert!(whole_output.contains("ai_edited_after_checkpoint"), "{whole_output}");
        assert!(whole_output.contains("inserted_after_checkpoint"), "{whole_output}");
    }
}

#[test]
fn test_malformed_checkpoints_in_pr_are_unverifiable() {
    let (repo, cfg) = setup_repo();
    std::fs::write(repo.join("base.txt"), "base\n").unwrap();
    git(&repo, &cfg, &["add", "base.txt"]);
    git(&repo, &cfg, &["commit", "-q", "-m", "base"]);
    let base = git(&repo, &cfg, &["rev-parse", "HEAD"]);
    std::fs::write(repo.join("g.txt"), "ai line\n").unwrap();
    git(&repo, &cfg, &["add", "g.txt"]);
    git(&repo, &cfg, &["commit", "-q", "-m", "feature"]);
    let head = git(&repo, &cfg, &["rev-parse", "HEAD"]);
    write_note(&repo, &cfg, &head, "g.txt\n  s_1::t_ai 1\n---\n{\"schema_version\":\"authorship/3.0.0\"}\n");
    write_checkpoints(&repo, &base, "not JSON\n");

    let out = prov(&repo, &cfg, &["--base", &base, "--head", &head, "--json"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["counts"]["probable"], 1);
    assert_eq!(v["counts"]["strong"], 0);
    assert_eq!(v["per_commit"][0]["checkpoints_found"], false);
    assert!(!v["per_commit"][0]["checkpoints_error"].as_str().unwrap().is_empty());
    assert!(v["warnings"].to_string().contains("raw checkpoints could not be read"));
    assert!(v["warnings"].to_string().contains("could NOT be verified"));
}

#[test]
fn test_revision_values_starting_with_dash_are_usage_errors() {
    let (repo, cfg) = setup_repo();
    for args in [
        vec!["--commit", "-HEAD"],
        vec!["--base", "-main", "--head", "main"],
        vec!["--base", "main", "--head", "-feature"],
    ] {
        let out = prov(&repo, &cfg, &args);
        assert_eq!(out.status.code(), Some(2), "args {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        assert!(String::from_utf8_lossy(&out.stderr).contains("value cannot start with"));
    }
}

#[test]
fn test_unsupported_note_schema_is_unknown_without_aborting() {
    let (repo, cfg) = setup_repo();
    std::fs::write(repo.join("base.txt"), "base\n").unwrap();
    git(&repo, &cfg, &["add", "base.txt"]);
    git(&repo, &cfg, &["commit", "-q", "-m", "base"]);
    let parent = git(&repo, &cfg, &["rev-parse", "HEAD"]);
    std::fs::write(repo.join("f.txt"), "new line\n").unwrap();
    git(&repo, &cfg, &["add", "f.txt"]);
    git(&repo, &cfg, &["commit", "-q", "-m", "unsupported note"]);
    let head = git(&repo, &cfg, &["rev-parse", "HEAD"]);
    write_note(&repo, &cfg, &head, "f.txt\n  s_1::t_ai 1\n---\n{\"schema_version\":\"authorship/9.0.0\"}\n");

    let out = prov(&repo, &cfg, &["--base", &parent, "--head", &head, "--json"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["counts"]["unknown"], 1);
    assert_eq!(v["eligible_added_lines"], 1);
    assert!(v["warnings"].to_string().contains("Git AI note could not be read"));
    assert!(v["per_commit"][0]["note_error"].as_str().unwrap().contains("unsupported schema_version"));
}

#[test]
fn test_unreadable_note_is_present_and_not_reported_missing() {
    let (repo, cfg) = setup_repo();
    std::fs::write(repo.join("base.txt"), "base\n").unwrap();
    git(&repo, &cfg, &["add", "base.txt"]);
    git(&repo, &cfg, &["commit", "-q", "-m", "base"]);
    let parent = git(&repo, &cfg, &["rev-parse", "HEAD"]);
    std::fs::write(repo.join("f.txt"), "new line\n").unwrap();
    git(&repo, &cfg, &["add", "f.txt"]);
    git(&repo, &cfg, &["commit", "-q", "-m", "unsupported note"]);
    let head = git(&repo, &cfg, &["rev-parse", "HEAD"]);
    write_note(&repo, &cfg, &head, "f.txt\n  s_1::t_ai 1\n---\n{\"schema_version\":\"authorship/9.0.0\"}\n");

    let out = prov(&repo, &cfg, &["--base", &parent, "--head", &head, "--json", "--explain"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["counts"]["unknown"], 1);
    assert_eq!(v["per_commit"][0]["note_found"], true);
    assert!(v["per_commit"][0]["note_error"].as_str().unwrap().contains("unsupported schema_version"));
    assert!(!v["warnings"].to_string().contains("no Git AI note"));
    assert_eq!(v["explanations"][0]["reason_code"], "note_unreadable");
    assert_eq!(v["explanations"][0]["reason"], "note could not be read -> unknown");
}

#[test]
fn test_survival_rule_rewritten_and_deleted_lines() {
    let (repo, cfg) = setup_repo();

    // Base commit
    std::fs::write(repo.join("lib.rs"), "fn root() {}\n").unwrap();
    git(&repo, &cfg, &["add", "lib.rs"]);
    git(&repo, &cfg, &["commit", "-q", "-m", "base"]);
    let base = git(&repo, &cfg, &["rev-parse", "HEAD"]);

    // Commit 1: adds lines 2-3 to lib.rs (AI), and creates temp.txt with 2 lines (AI)
    std::fs::write(
        repo.join("lib.rs"),
        "fn root() {}\nfn c1_keep() {}\nfn c1_rewrite_me() {}\n",
    )
    .unwrap();
    std::fs::write(repo.join("temp.txt"), "t1\nt2\n").unwrap();
    git(&repo, &cfg, &["add", "lib.rs", "temp.txt"]);
    git(&repo, &cfg, &["commit", "-q", "-m", "commit 1"]);
    let c1 = git(&repo, &cfg, &["rev-parse", "HEAD"]);

    let c1_note = "\
lib.rs
  s_1::t_c1 2-3
temp.txt
  s_1::t_c1 1-2
---
{\"schema_version\":\"authorship/3.0.0\"}
";
    write_note(&repo, &cfg, &c1, c1_note);
    write_checkpoints(
        &repo,
        &base,
        "{\"kind\":\"AiAgent\",\"trace_id\":\"t_c1\"}\n",
    );

    // Commit 2: rewrites line 3 of lib.rs (human, no AI note claim), and deletes temp.txt
    std::fs::write(
        repo.join("lib.rs"),
        "fn root() {}\nfn c1_keep() {}\nfn c2_rewritten() {}\n",
    )
    .unwrap();
    git(&repo, &cfg, &["rm", "temp.txt"]);
    git(&repo, &cfg, &["add", "lib.rs"]);
    git(&repo, &cfg, &["commit", "-q", "-m", "commit 2 rewrites line 3 and deletes temp"]);
    let c2 = git(&repo, &cfg, &["rev-parse", "HEAD"]);

    // Commit 2 note has empty/no AI attestation for lib.rs
    let c2_note = "\
lib.rs
---
{\"schema_version\":\"authorship/3.0.0\"}
";
    write_note(&repo, &cfg, &c2, c2_note);

    // Analyze PR
    let out = prov(
        &repo,
        &cfg,
        &["--base", &base, "--head", &c2, "--json", "--explain"],
    );
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();

    // Surviving added lines in HEAD are:
    // lib.rs line 2 (from c1: ai_strong)
    // lib.rs line 3 (from c2: unknown)
    // temp.txt is deleted, 0 lines.
    assert_eq!(v["eligible_added_lines"], 2);
    assert_eq!(v["counts"]["strong"], 1);
    assert_eq!(v["counts"]["unknown"], 1);

    let per_commit = v["per_commit"].as_array().unwrap();
    assert_eq!(per_commit[0]["lines_in_final"], 1); // c1 only kept 1 line!
    assert_eq!(per_commit[1]["lines_in_final"], 1); // c2 authored the rewritten line!

    let explanations = v["explanations"].as_array().unwrap();
    assert_eq!(explanations.len(), 2);

    assert_eq!(explanations[0]["file"], "lib.rs");
    assert_eq!(explanations[0]["line"], 2);
    assert_eq!(explanations[0]["commit"], c1);
    assert_eq!(explanations[0]["category"], "ai_strong");

    assert_eq!(explanations[1]["file"], "lib.rs");
    assert_eq!(explanations[1]["line"], 3);
    assert_eq!(explanations[1]["commit"], c2);
    assert_eq!(explanations[1]["category"], "unknown");
}

#[test]
fn test_unbacked_claims_in_pr_downgraded() {
    let (repo, cfg) = setup_repo();

    std::fs::write(repo.join("f.txt"), "base\n").unwrap();
    git(&repo, &cfg, &["add", "f.txt"]);
    git(&repo, &cfg, &["commit", "-q", "-m", "base"]);
    let base = git(&repo, &cfg, &["rev-parse", "HEAD"]);

    std::fs::write(repo.join("f.txt"), "base\nline1\nline2\n").unwrap();
    git(&repo, &cfg, &["add", "f.txt"]);
    git(&repo, &cfg, &["commit", "-q", "-m", "unbacked AI claims"]);
    let c1 = git(&repo, &cfg, &["rev-parse", "HEAD"]);

    // Note claims trace t_ghost
    let c1_note = "\
f.txt
  s_1::t_ghost 2-3
---
{\"schema_version\":\"authorship/3.0.0\"}
";
    write_note(&repo, &cfg, &c1, c1_note);
    // Checkpoints file only has trace t_real, so t_ghost is unbacked
    write_checkpoints(
        &repo,
        &base,
        "{\"kind\":\"AiAgent\",\"trace_id\":\"t_real\"}\n",
    );

    let out = prov(
        &repo,
        &cfg,
        &["--base", &base, "--head", &c1, "--json", "--explain"],
    );
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();

    assert_eq!(v["eligible_added_lines"], 2);
    assert_eq!(v["counts"]["strong"], 0);
    assert_eq!(v["counts"]["unknown"], 2);
    assert_eq!(v["unbacked_ai_lines"], 2);

    let warnings = v["warnings"].as_array().unwrap();
    assert!(
        warnings
            .iter()
            .any(|w| w.as_str().unwrap().contains("unbacked AI claim(s) counted as unknown")),
        "warnings: {warnings:?}"
    );

    let expl = v["explanations"].as_array().unwrap();
    assert!(expl[0]["reason"]
        .as_str()
        .unwrap()
        .contains("not in raw checkpoints -> unknown"));
}

#[test]
fn test_missing_notes_on_commit_in_range() {
    let (repo, cfg) = setup_repo();

    std::fs::write(repo.join("f.txt"), "base\n").unwrap();
    git(&repo, &cfg, &["add", "f.txt"]);
    git(&repo, &cfg, &["commit", "-q", "-m", "base"]);
    let base = git(&repo, &cfg, &["rev-parse", "HEAD"]);

    std::fs::write(repo.join("f.txt"), "base\nline1\nline2\n").unwrap();
    git(&repo, &cfg, &["add", "f.txt"]);
    git(&repo, &cfg, &["commit", "-q", "-m", "no note commit"]);
    let c1 = git(&repo, &cfg, &["rev-parse", "HEAD"]);

    // Do NOT create any note for c1

    let out = prov(
        &repo,
        &cfg,
        &["--base", &base, "--head", &c1, "--json", "--explain"],
    );
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();

    assert_eq!(v["eligible_added_lines"], 2);
    assert_eq!(v["counts"]["unknown"], 2);

    let per_commit = v["per_commit"].as_array().unwrap();
    assert_eq!(per_commit[0]["note_found"], false);
    assert_eq!(per_commit[0]["line_check"], "not_applicable");

    let warnings = v["warnings"].as_array().unwrap();
    assert!(
        warnings
            .iter()
            .any(|w| w.as_str().unwrap().contains("no Git AI note")),
        "warnings: {warnings:?}"
    );

    let expl = v["explanations"].as_array().unwrap();
    assert_eq!(expl[0]["category"], "unknown");
    assert_eq!(expl[0]["reason"], "no note -> unknown");
}

#[test]
fn prompt_claim_warns_when_raw_checkpoints_are_missing_in_pr_range_mode() {
    let (repo, cfg) = setup_repo();
    std::fs::write(repo.join("f.txt"), "base\n").unwrap();
    git(&repo, &cfg, &["add", "f.txt"]);
    git(&repo, &cfg, &["commit", "-q", "-m", "base"]);
    let base = git(&repo, &cfg, &["rev-parse", "HEAD"]);
    std::fs::write(repo.join("f.txt"), "base\nadded\n").unwrap();
    git(&repo, &cfg, &["add", "f.txt"]);
    git(&repo, &cfg, &["commit", "-q", "-m", "prompt claim"]);
    let head = git(&repo, &cfg, &["rev-parse", "HEAD"]);
    write_note(&repo, &cfg, &head, "f.txt\n  prompt_1 2\n---\n{\"schema_version\":\"authorship/3.0.0\"}\n");

    let out = prov(&repo, &cfg, &["--base", &base, "--head", &head, "--json", "--explain"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(v["warnings"].to_string().contains("AI claims could NOT be verified"));
    assert_eq!(v["counts"]["probable"], 1);
    assert_eq!(v["explanations"][0]["reason_code"], "ai_unverified");
}

#[test]
fn test_excluded_files_ignored_in_metric() {
    let (repo, cfg) = setup_repo();

    std::fs::write(repo.join("f.rs"), "base\n").unwrap();
    git(&repo, &cfg, &["add", "f.rs"]);
    git(&repo, &cfg, &["commit", "-q", "-m", "base"]);
    let base = git(&repo, &cfg, &["rev-parse", "HEAD"]);

    // Add 2 lines to f.rs (AI) and 5 lines to Cargo.lock (excluded)
    std::fs::write(repo.join("f.rs"), "base\nline1\nline2\n").unwrap();
    std::fs::write(repo.join("Cargo.lock"), "l1\nl2\nl3\nl4\nl5\n").unwrap();
    git(&repo, &cfg, &["add", "f.rs", "Cargo.lock"]);
    git(&repo, &cfg, &["commit", "-q", "-m", "feat with lockfile"]);
    let c1 = git(&repo, &cfg, &["rev-parse", "HEAD"]);

    let c1_note = "\
f.rs
  s_1::t_c1 2-3
---
{\"schema_version\":\"authorship/3.0.0\"}
";
    write_note(&repo, &cfg, &c1, c1_note);
    write_checkpoints(
        &repo,
        &base,
        "{\"kind\":\"AiAgent\",\"trace_id\":\"t_c1\"}\n",
    );

    let out = prov(
        &repo,
        &cfg,
        &["--base", &base, "--head", &c1, "--json", "--explain"],
    );
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();

    assert_eq!(v["eligible_added_lines"], 2);
    assert_eq!(v["counts"]["excluded"], 5);
    assert_eq!(v["counts"]["strong"], 2);
    let det = v["ai_deterministic_pct"].as_f64().unwrap();
    assert!((det - 100.0).abs() < 1e-3);

    let expl = v["explanations"].as_array().unwrap();
    let lock_expls: Vec<_> = expl
        .iter()
        .filter(|e| e["file"] == "Cargo.lock")
        .collect();
    assert_eq!(lock_expls.len(), 5);
    assert_eq!(lock_expls[0]["category"], "excluded");
}

#[test]
fn test_empty_prs_and_edge_cases() {
    let (repo, cfg) = setup_repo();

    std::fs::write(repo.join("f.txt"), "base\n").unwrap();
    git(&repo, &cfg, &["add", "f.txt"]);
    git(&repo, &cfg, &["commit", "-q", "-m", "base"]);
    let base = git(&repo, &cfg, &["rev-parse", "HEAD"]);

    // Empty PR: 0 commits (base..base)
    let out = prov(&repo, &cfg, &["--base", &base, "--head", &base, "--json"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["commits_in_range"], 0);
    assert_eq!(v["eligible_added_lines"], 0);
    assert!(v["ai_deterministic_pct"].is_null());
    assert!(v["ai_linked_pct"].is_null());

    // Single commit mode default (HEAD) still works
    let out_single = prov(&repo, &cfg, &["--json"]);
    assert!(out_single.status.success());

    // --explain without --base/--head errors out with clean usage error
    let out_bad_explain = prov(&repo, &cfg, &["--explain"]);
    assert_eq!(out_bad_explain.status.code(), Some(2));
    let err_str = String::from_utf8_lossy(&out_bad_explain.stderr);
    assert!(err_str.contains("--explain requires --base and --head"));

    // --commit combined with --base/--head errors out
    let out_bad_comb = prov(
        &repo,
        &cfg,
        &["--commit", "HEAD", "--base", &base, "--head", &base],
    );
    assert_eq!(out_bad_comb.status.code(), Some(2));
    let err_str = String::from_utf8_lossy(&out_bad_comb.stderr);
    assert!(err_str.contains("--commit cannot be combined with --base and --head"));

    // Invalid base/head revisions error out with exit code 1
    let out_bad_rev = prov(
        &repo,
        &cfg,
        &["--base", "nonexistent_rev", "--head", &base],
    );
    assert_eq!(out_bad_rev.status.code(), Some(1));
}

#[test]
fn test_merge_commits_rejected() {
    let (repo, cfg) = setup_repo();

    std::fs::write(repo.join("f.txt"), "base\n").unwrap();
    git(&repo, &cfg, &["add", "f.txt"]);
    git(&repo, &cfg, &["commit", "-q", "-m", "base"]);
    let base = git(&repo, &cfg, &["rev-parse", "HEAD"]);

    // Create branch 1
    git(&repo, &cfg, &["checkout", "-b", "branch1"]);
    std::fs::write(repo.join("b1.txt"), "b1\n").unwrap();
    git(&repo, &cfg, &["add", "b1.txt"]);
    git(&repo, &cfg, &["commit", "-q", "-m", "b1 commit"]);

    // Create branch 2
    git(&repo, &cfg, &["checkout", "main"]);
    git(&repo, &cfg, &["checkout", "-b", "branch2"]);
    std::fs::write(repo.join("b2.txt"), "b2\n").unwrap();
    git(&repo, &cfg, &["add", "b2.txt"]);
    git(&repo, &cfg, &["commit", "-q", "-m", "b2 commit"]);

    // Merge branch1 into branch2
    git(&repo, &cfg, &["merge", "--no-ff", "-m", "merge branch1", "branch1"]);
    let merge_head = git(&repo, &cfg, &["rev-parse", "HEAD"]);

    // Analyze PR containing merge commit
    let out = prov(&repo, &cfg, &["--base", &base, "--head", &merge_head]);
    assert_eq!(out.status.code(), Some(1));
    let err_str = String::from_utf8_lossy(&out.stderr);
    assert!(err_str.contains("merge commits are not supported yet"));
}

#[test]
fn test_text_report_pr_range() {
    let (repo, cfg) = setup_repo();

    std::fs::write(repo.join("f.txt"), "base\n").unwrap();
    git(&repo, &cfg, &["add", "f.txt"]);
    git(&repo, &cfg, &["commit", "-q", "-m", "base"]);
    let base = git(&repo, &cfg, &["rev-parse", "HEAD"]);

    std::fs::write(repo.join("f.txt"), "base\nline1\nline2\n").unwrap();
    git(&repo, &cfg, &["add", "f.txt"]);
    git(&repo, &cfg, &["commit", "-q", "-m", "feat commit"]);
    let c1 = git(&repo, &cfg, &["rev-parse", "HEAD"]);

    let c1_note = "\
f.txt
  s_1::t_c1 2-3
---
{\"schema_version\":\"authorship/3.0.0\"}
";
    write_note(&repo, &cfg, &c1, c1_note);
    write_checkpoints(
        &repo,
        &base,
        "{\"kind\":\"AiAgent\",\"trace_id\":\"t_c1\"}\n",
    );

    let out = prov(&repo, &cfg, &["--base", &base, "--head", &c1, "--explain"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("PR range:"));
    assert!(s.contains("Eligible added lines: 2"));
    assert!(s.contains("AI deterministic: 100.0%"));
    assert!(s.contains("checkpoints:found"));
    assert!(s.contains("--- Per-line explanations ---"));
    assert!(s.contains("f.txt:"));
    assert!(s.contains("L2: ai_strong"));
    assert!(s.contains("L3: ai_strong"));
}

#[test]
fn test_unattributed_lines_reported_and_counted_as_unknown() {
    let (repo, cfg) = setup_repo();

    std::fs::write(repo.join("f.txt"), "base\n").unwrap();
    git(&repo, &cfg, &["add", "f.txt"]);
    git(&repo, &cfg, &["commit", "-q", "-m", "base"]);
    let base = git(&repo, &cfg, &["rev-parse", "HEAD"]);

    // A gitlink (submodule mode) appears as an added line in the diff, but `git blame`
    // cannot attribute it — counted as unattributed/unknown.
    git(
        &repo,
        &cfg,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            "160000",
            "1234567890123456789012345678901234567890",
            "mysub",
        ],
    );
    git(&repo, &cfg, &["commit", "-q", "-m", "c1 with gitlink"]);
    let c1 = git(&repo, &cfg, &["rev-parse", "HEAD"]);

    let out = prov(
        &repo,
        &cfg,
        &["--base", &base, "--head", &c1, "--json", "--explain"],
    );
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();

    assert_eq!(v["unattributed_lines"], 1);
    assert_eq!(v["counts"]["unknown"], 1);
    assert_eq!(v["eligible_added_lines"], 1);

    let warnings = v["warnings"].as_array().unwrap();
    assert!(
        warnings.iter().any(|w| w
            .as_str()
            .unwrap()
            .contains("1 added line(s) could not be attributed to a commit in the range: counted as unknown")),
        "warnings: {warnings:?}"
    );

    let expl = v["explanations"].as_array().unwrap();
    assert_eq!(expl.len(), 1);
    assert_eq!(expl[0]["category"], "unknown");
    assert_eq!(expl[0]["reason_code"], "unattributed");
}

#[test]
fn test_merge_base_three_dot_ignores_main_side_edits() {
    let (repo, cfg) = setup_repo();

    // Base: f.txt with three lines
    std::fs::write(repo.join("f.txt"), "line1\nline2\nline3\n").unwrap();
    git(&repo, &cfg, &["add", "f.txt"]);
    git(&repo, &cfg, &["commit", "-q", "-m", "base"]);
    let _fork = git(&repo, &cfg, &["rev-parse", "HEAD"]);

    // Feature branch: new file g.txt with 2 AI lines
    git(&repo, &cfg, &["checkout", "-b", "feature"]);
    std::fs::write(repo.join("g.txt"), "g1\ng2\n").unwrap();
    git(&repo, &cfg, &["add", "g.txt"]);
    git(&repo, &cfg, &["commit", "-q", "-m", "feature adds g"]);
    let feat = git(&repo, &cfg, &["rev-parse", "HEAD"]);
    let feat_note = "\
g.txt
  s_1::t_feat 1-2
---
{\"schema_version\":\"authorship/3.0.0\"}
";
    write_note(&repo, &cfg, &feat, feat_note);
    write_checkpoints(
        &repo,
        &_fork,
        "{\"kind\":\"AiAgent\",\"trace_id\":\"t_feat\"}\n",
    );

    // On main (after the fork): change line 2 of f.txt
    git(&repo, &cfg, &["checkout", "main"]);
    std::fs::write(repo.join("f.txt"), "line1\nmain edit\nline3\n").unwrap();
    git(&repo, &cfg, &["add", "f.txt"]);
    git(&repo, &cfg, &["commit", "-q", "-m", "main edit line2"]);

    // With three-dot (merge-base) semantics, only g.txt's 2 lines are in the PR.
    // A two-dot diff would wrongly treat the untouched old line2 as an added line.
    let out = prov(
        &repo,
        &cfg,
        &["--base", "main", "--head", "feature", "--json"],
    );
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();

    assert_eq!(v["eligible_added_lines"], 2);
    assert_eq!(v["unattributed_lines"], 0);
    assert_eq!(v["counts"]["strong"], 2);
}
