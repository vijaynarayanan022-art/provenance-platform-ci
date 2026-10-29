use serde_json::json;
use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

fn main() {
    let temp = std::env::temp_dir().join(format!("prov-bench-{}-{}", std::process::id(), now()));
    let repo = temp.join("repo");
    let home = temp.join("store");
    fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init"]);
    git(&repo, &["config", "user.email", "bench@example.invalid"]);
    git(&repo, &["config", "user.name", "Benchmark"]);
    fs::write(repo.join("README.txt"), "benchmark repository\n").unwrap();
    git(&repo, &["add", "README.txt"]);
    git(&repo, &["commit", "-m", "benchmark baseline"]);
    for i in 0..20_000 {
        fs::write(repo.join(format!("file-{i:05}.txt")), format!("line {i}\n")).unwrap();
    }
    let cold = timed_hook(&repo, &home, "PreToolUse", "bench", "cold");
    for i in 0..200 {
        fs::write(
            repo.join(format!("file-{i:05}.txt")),
            format!("line {i}\nchanged\n"),
        )
        .unwrap();
    }
    let warm = timed_hook(&repo, &home, "PostToolUse", "bench", "cold");
    println!(
        "20,000 untracked files plus README; 200 files changed\ncold scan hook: {} ms\nwarm scan hook: {} ms",
        cold.as_millis(),
        warm.as_millis()
    );
    use std::io::Write as _;
    let _ = std::io::stdout().flush();
    let _ = fs::remove_dir_all(temp);
}
fn timed_hook(
    repo: &Path,
    home: &Path,
    event: &str,
    session: &str,
    tool: &str,
) -> std::time::Duration {
    let input = json!({"hook_event_name": event, "session_id": session, "turn_id": "bench", "cwd": repo,
        "model": "benchmark", "tool_name": "PowerShell", "tool_use_id": tool, "tool_input": {"command": "benchmark"}});
    std::env::set_var("PROV_HOME", home);
    let start = Instant::now();
    prov_capture::process_json(input.to_string().as_bytes());
    start.elapsed()
}
fn git(repo: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(repo)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success());
}
fn now() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}
