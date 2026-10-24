//! End to end: rebuild the real Codex lab scenario in a temp git repo (real note + real raw
//! checkpoint fixtures) and run the `prov analyze` binary on it.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Unique per call. The clock alone is not enough: on Windows its resolution is coarse, so two
/// parallel tests can get the same timestamp and `git init` the same folder at once.
static COUNTER: AtomicUsize = AtomicUsize::new(0);

fn git(dir: &Path, empty_cfg: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C").arg(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", empty_cfg) // ignore the user's global trace2 / Git AI daemon
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Lab").env("GIT_AUTHOR_EMAIL", "lab@example.com")
        .env("GIT_COMMITTER_NAME", "Lab").env("GIT_COMMITTER_EMAIL", "lab@example.com")
        .output().expect("git runs");
    assert!(out.status.success(), "git {:?}: {}", args, String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn setup() -> (PathBuf, PathBuf) {
    let id = format!("{}-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos(), COUNTER.fetch_add(1, Ordering::SeqCst));
    let dir = std::env::temp_dir().join(format!("prov-cli-test-{id}"));
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = dir.join("empty.gitconfig");
    std::fs::write(&cfg, "").unwrap();
    let repo = dir.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &cfg, &["init", "-q", "-b", "main"]);
    git(&repo, &cfg, &["config", "core.autocrlf", "false"]);
    std::fs::write(repo.join("notes.txt"), "human line 1\nhuman line 2\nhuman line 3\n").unwrap();
    git(&repo, &cfg, &["add", "notes.txt"]);
    git(&repo, &cfg, &["commit", "-q", "-m", "base"]);
    let base = git(&repo, &cfg, &["rev-parse", "HEAD"]);
    std::fs::write(repo.join("notes.txt"), "human line 1\nhuman line 2\nhuman line 3\ncodex line 1\ncodex line 2\ncodex line 3\ncodex line 4\nmy line 1\nmy line 2\n").unwrap();
    git(&repo, &cfg, &["add", "notes.txt"]);
    git(&repo, &cfg, &["commit", "-q", "-m", "codex plus human"]);
    // attach the REAL Git AI note and the REAL raw checkpoint file from the lab
    let note = Path::new(env!("CARGO_MANIFEST_DIR")).join("../prov-gitai/tests/fixtures/real1-codex-note.txt");
    git(&repo, &cfg, &["notes", "--ref=ai", "add", "-f", "-F", note.to_str().unwrap(), "HEAD"]);
    let wl = repo.join(".git/ai/working_logs").join(format!("old-{base}"));
    std::fs::create_dir_all(&wl).unwrap();
    let cp = Path::new(env!("CARGO_MANIFEST_DIR")).join("../prov-gitai/tests/fixtures/real1-checkpoints.jsonl");
    std::fs::copy(cp, wl.join("checkpoints.jsonl")).unwrap();
    (repo, cfg)
}

fn prov(repo: &Path, cfg: &Path, extra: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_prov"))
        .arg("analyze").arg("--repo").arg(repo).args(extra)
        .env("GIT_CONFIG_GLOBAL", cfg).env("GIT_CONFIG_NOSYSTEM", "1")
        .output().unwrap()
}

#[test]
fn analyze_downgrades_unbacked_human_lines() {
    let (repo, cfg) = setup();
    let out = prov(&repo, &cfg, &["--json"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["schema_version"], "prov.analysis/0.1");
    assert_eq!(v["eligible_added_lines"], 6);
    assert_eq!(v["counts"]["strong"], 4);
    assert_eq!(v["counts"]["unknown"], 2);
    assert_eq!(v["unbacked_ai_lines"], 2);
    assert_eq!(v["raw_checkpoints"]["found"], true);
    let det = v["ai_deterministic_pct"].as_f64().unwrap();
    assert!((det - 66.6666666).abs() < 1e-3, "{det}");
}

#[test]
fn analyze_warns_when_raw_checkpoints_are_missing() {
    let (repo, cfg) = setup();
    std::fs::remove_dir_all(repo.join(".git/ai")).unwrap();
    let out = prov(&repo, &cfg, &["--json"]);
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["raw_checkpoints"]["found"], false);
    assert_eq!(v["counts"]["strong"], 6, "unverifiable: claims are kept but flagged");
    assert!(v["warnings"].to_string().contains("could NOT be verified"));
}

#[test]
fn analyze_human_only_note_does_not_warn_about_missing_checkpoints() {
    let (repo, cfg) = setup();
    let note_path = repo.join(".git/human-only-note.txt");
    std::fs::write(
        &note_path,
        "notes.txt\n  h_human 4-9\n---\n{\"schema_version\":\"authorship/3.0.0\"}\n",
    )
    .unwrap();
    git(&repo, &cfg, &["notes", "--ref=ai", "add", "-f", "-F", note_path.to_str().unwrap(), "HEAD"]);
    std::fs::remove_dir_all(repo.join(".git/ai")).unwrap();

    let out = prov(&repo, &cfg, &["--json"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["known_human_lines"], 6);
    assert_eq!(v["counts"]["unknown"], 6);
    assert!(!v["warnings"].to_string().contains("raw Git AI checkpoints not found"));
    assert!(!v["warnings"].to_string().contains("could NOT be verified"));
}

#[test]
fn analyze_unreadable_note_has_note_unreadable_reason() {
    let (repo, cfg) = setup();
    let note_path = repo.join(".git/unreadable-note.txt");
    std::fs::write(
        &note_path,
        "notes.txt\n  s_1::t_ai 4-9\n---\n{\"schema_version\":\"authorship/9.0.0\"}\n",
    )
    .unwrap();
    git(&repo, &cfg, &["notes", "--ref=ai", "add", "-f", "-F", note_path.to_str().unwrap(), "HEAD"]);

    let out = prov(&repo, &cfg, &["--json"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["note_found"], true);
    assert!(v["warnings"].to_string().contains("Git AI note could not be read"));
    assert!(!v["warnings"].to_string().contains("no Git AI note"));
    assert_eq!(v["explanations"][0]["reason_code"], "note_unreadable");
    assert_eq!(v["explanations"][0]["reason"], "note could not be read -> unknown");
}

#[test]
fn analyze_malformed_raw_checkpoints_are_unverifiable() {
    let (repo, cfg) = setup();
    let parent = git(&repo, &cfg, &["rev-parse", "HEAD^"]);
    let checkpoint_file = repo
        .join(".git/ai/working_logs")
        .join(format!("old-{parent}"))
        .join("checkpoints.jsonl");
    std::fs::write(checkpoint_file, "not JSON\n").unwrap();

    let out = prov(&repo, &cfg, &["--json"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["counts"]["strong"], 6);
    assert_eq!(v["raw_checkpoints"]["found"], false);
    assert!(!v["raw_checkpoints"]["error"].as_str().unwrap().is_empty());
    assert!(v["warnings"].to_string().contains("raw checkpoints could not be read"));
    assert!(v["warnings"].to_string().contains("could NOT be verified"));
}

#[test]
fn analyze_text_report_and_usage_error() {
    let (repo, cfg) = setup();
    let out = prov(&repo, &cfg, &[]);
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("AI deterministic: 66.7%"), "{s}");
    let bad = Command::new(env!("CARGO_BIN_EXE_prov")).arg("nope").output().unwrap();
    assert_eq!(bad.status.code(), Some(2));
}
