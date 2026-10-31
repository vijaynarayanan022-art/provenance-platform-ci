use serde_json::json;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const REPEATS: usize = 5;
const GIT_AI: &str = r"C:\dev\git-ai-bin\git-ai.exe";
struct Case {
    name: &'static str,
    tracked: usize,
    untracked: usize,
    edits: usize,
}
struct Timing {
    pre: Vec<Duration>,
    post: Vec<Duration>,
    status: Vec<Duration>,
    complete: usize,
    pre_phases: Vec<prov_capture::TimingSnapshot>,
    post_phases: Vec<prov_capture::TimingSnapshot>,
    pre_start: Vec<Duration>,
    post_start: Vec<Duration>,
}

fn main() {
    let binary = std::env::current_exe()
        .expect("current exe")
        .parent()
        .and_then(Path::parent)
        .unwrap()
        .join(if cfg!(windows) {
            "prov-hook.exe"
        } else {
            "prov-hook"
        });
    assert!(
        binary.exists(),
        "build the binary first with cargo build -p prov-hook --bin prov-hook"
    );
    let cases = [
        Case {
            name: "1k tracked clean, 0 changed",
            tracked: 1_000,
            untracked: 0,
            edits: 0,
        },
        Case {
            name: "1k tracked, 50 changed",
            tracked: 1_000,
            untracked: 0,
            edits: 50,
        },
        Case {
            name: "5k tracked clean, 0 changed",
            tracked: 5_000,
            untracked: 0,
            edits: 0,
        },
        Case {
            name: "5k tracked, 50 changed",
            tracked: 5_000,
            untracked: 0,
            edits: 50,
        },
        Case {
            name: "a: 20,000 tracked clean, 0 changed",
            tracked: 20_000,
            untracked: 0,
            edits: 0,
        },
        Case {
            name: "b: 20,000 tracked clean, 200 changed",
            tracked: 20_000,
            untracked: 0,
            edits: 200,
        },
        Case {
            name: "c: 20,000 untracked, 0 changed",
            tracked: 0,
            untracked: 20_000,
            edits: 0,
        },
        Case {
            name: "d: 20,000 tracked + 5,000 untracked, 200 changed",
            tracked: 20_000,
            untracked: 5_000,
            edits: 200,
        },
    ];
    println!("Benchmark: {REPEATS} repeats per case and execution mode; all times in ms (median / worst). Scan complete counts report out of {REPEATS}.");
    println!(
        "Case | In-process Pre | In-process Post | Real binary Pre | Real binary Post | git status"
    );
    let base = std::env::temp_dir().join(format!("prov-bench-{}-{}", std::process::id(), now()));
    let tracked_repo = base.join("tracked-repo");
    let tracked_1k = base.join("tracked-1k");
    let tracked_5k = base.join("tracked-5k");
    let untracked_repo = base.join("untracked-repo");
    fs::create_dir_all(&tracked_repo).unwrap();
    fs::create_dir_all(&tracked_1k).unwrap();
    fs::create_dir_all(&tracked_5k).unwrap();
    fs::create_dir_all(&untracked_repo).unwrap();
    initialize(
        &tracked_repo,
        &Case {
            name: "tracked-base",
            tracked: 20_000,
            untracked: 0,
            edits: 0,
        },
    );
    initialize(
        &tracked_1k,
        &Case {
            name: "1k",
            tracked: 1_000,
            untracked: 0,
            edits: 0,
        },
    );
    initialize(
        &tracked_5k,
        &Case {
            name: "5k",
            tracked: 5_000,
            untracked: 0,
            edits: 0,
        },
    );
    initialize(
        &untracked_repo,
        &Case {
            name: "untracked-base",
            tracked: 0,
            untracked: 20_000,
            edits: 0,
        },
    );
    for (index, case) in cases.iter().enumerate() {
        let repo = match case.tracked {
            1_000 => &tracked_1k,
            5_000 => &tracked_5k,
            0 => &untracked_repo,
            _ => &tracked_repo,
        };
        if index == 7 {
            for i in 0..5_000 {
                fs::write(
                    repo.join(format!("extra-untracked-{i:05}.txt")),
                    format!("extra {i}\n"),
                )
                .unwrap();
            }
        }
        let mut in_process = Timing {
            pre: vec![],
            post: vec![],
            status: vec![],
            complete: 0,
            pre_phases: vec![],
            post_phases: vec![],
            pre_start: vec![],
            post_start: vec![],
        };
        let mut real = Timing {
            pre: vec![],
            post: vec![],
            status: vec![],
            complete: 0,
            pre_phases: vec![],
            post_phases: vec![],
            pre_start: vec![],
            post_start: vec![],
        };
        for repeat in 0..REPEATS {
            reset_edits(repo, case.edits);
            let store = base.join(format!("store-{index}-in"));
            let (in_pre, in_pre_phases) = run_in_process(repo, &store, "PreToolUse", repeat);
            in_process.pre.push(in_pre);
            in_process.pre_phases.push(in_pre_phases);
            apply_edits(repo, case.edits);
            let (in_post, in_post_phases) = run_in_process(repo, &store, "PostToolUse", repeat);
            in_process.post.push(in_post);
            in_process.post_phases.push(in_post_phases);
            if complete(&store) {
                in_process.complete += 1;
            }
            reset_edits(repo, case.edits);
            let store = base.join(format!("store-{index}-bin"));
            let real_pre = run_binary(&binary, repo, &store, "PreToolUse", repeat);
            real.pre.push(real_pre);
            real.pre_phases.push(in_pre_phases);
            real.pre_start
                .push(real_pre.saturating_sub(Duration::from_nanos(in_pre_phases.total_ns)));
            apply_edits(repo, case.edits);
            let real_post = run_binary(&binary, repo, &store, "PostToolUse", repeat);
            real.post.push(real_post);
            real.post_phases.push(in_post_phases);
            real.post_start
                .push(real_post.saturating_sub(Duration::from_nanos(in_post_phases.total_ns)));
            if complete(&store) {
                real.complete += 1;
            }
            reset_edits(repo, case.edits);
            let start = Instant::now();
            let status = Command::new("git")
                .args([
                    "-c",
                    "core.quotepath=false",
                    "status",
                    "--porcelain=v2",
                    "-z",
                    "--untracked-files=all",
                ])
                .current_dir(repo)
                .env("GIT_OPTIONAL_LOCKS", "0")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap();
            assert!(status.success());
            in_process.status.push(start.elapsed());
        }
        println!(
            "{} | {} | {} | {} | {} | {}",
            case.name,
            summary(&in_process.pre),
            summary(&in_process.post),
            summary(&real.pre),
            summary(&real.post),
            summary(&in_process.status)
        );
        println!("  breakdown ms medians (paired in-process phases; binary process-start is binary wall minus paired in-process total)");
        println!("  Pre: process-start {:.3}, discovery {:.3}, lock {:.3}, status {:.3}, hashing/snapshots {:.3}, writes {:.3}, state/shadow sync within writes {:.3}, log {:.3}", median_ms(&real.pre_start), phase_median(&in_process.pre_phases, |p|p.repo_discovery_ns), phase_median(&in_process.pre_phases, |p|p.lock_ns), phase_median(&in_process.pre_phases, |p|p.git_status_ns), phase_median(&in_process.pre_phases, |p|p.hashing_snapshots_ns), phase_median(&in_process.pre_phases, |p|p.writes_ns), phase_median(&in_process.pre_phases, |p|p.sync_ns), phase_median(&in_process.pre_phases, |p|p.log_work_ns));
        println!("  Post: process-start {:.3}, discovery {:.3}, lock {:.3}, status {:.3}, hashing/snapshots {:.3}, writes {:.3}, state/shadow sync within writes {:.3}, log {:.3}", median_ms(&real.post_start), phase_median(&in_process.post_phases, |p|p.repo_discovery_ns), phase_median(&in_process.post_phases, |p|p.lock_ns), phase_median(&in_process.post_phases, |p|p.git_status_ns), phase_median(&in_process.post_phases, |p|p.hashing_snapshots_ns), phase_median(&in_process.post_phases, |p|p.writes_ns), phase_median(&in_process.post_phases, |p|p.sync_ns), phase_median(&in_process.post_phases, |p|p.log_work_ns));
        println!(
            "  uninstrumented remainder median ms Pre/Post: {:.3} / {:.3}",
            phase_remainder(&in_process.pre_phases),
            phase_remainder(&in_process.post_phases)
        );
        println!(
            "  scan complete: in-process {}/{}, real binary {}/{}",
            in_process.complete, REPEATS, real.complete, REPEATS
        );
        println!("  git processes, direct median Pre/Post: {} / {}; paired real-event direct median Pre/Post: {} / {}", median_u64(&in_process.pre_phases,|p|p.git_processes), median_u64(&in_process.post_phases,|p|p.git_processes), median_u64(&real.pre_phases,|p|p.git_processes), median_u64(&real.post_phases,|p|p.git_processes));
        if index == 2 || index == 4 {
            run_fsmonitor_experiment(repo, case.tracked, &base);
        }
        run_git_ai_baseline(repo, case.tracked);
        let _ = std::io::stdout().flush();
    }
    let _ = fs::remove_dir_all(base);
}

fn initialize(repo: &Path, case: &Case) {
    git(repo, &["init"]);
    git(repo, &["config", "user.email", "bench@example.invalid"]);
    git(repo, &["config", "user.name", "Benchmark"]);
    for i in 0..case.tracked {
        fs::write(
            repo.join(format!("tracked-{i:05}.txt")),
            format!("original {i}\nline two\n"),
        )
        .unwrap();
    }
    if case.tracked > 0 {
        git(repo, &["add", "--all"]);
        git(repo, &["commit", "-m", "benchmark baseline"]);
    } else {
        fs::write(repo.join("README.txt"), "untracked benchmark baseline\n").unwrap();
        git(repo, &["add", "README.txt"]);
        git(repo, &["commit", "-m", "benchmark baseline"]);
    }
    for i in 0..case.untracked {
        fs::write(
            repo.join(format!("untracked-{i:05}.txt")),
            format!("untracked {i}\n"),
        )
        .unwrap();
    }
}
fn reset_edits(repo: &Path, edits: usize) {
    for i in 0..edits {
        fs::write(
            repo.join(format!("tracked-{i:05}.txt")),
            format!("original {i}\nline two\n"),
        )
        .unwrap();
    }
}
fn apply_edits(repo: &Path, edits: usize) {
    for i in 0..edits {
        fs::write(
            repo.join(format!("tracked-{i:05}.txt")),
            format!("original {i}\nline changed\n"),
        )
        .unwrap();
    }
}
fn payload(repo: &Path, event: &str, repeat: usize) -> Vec<u8> {
    json!({"hook_event_name":event,"session_id":format!("bench-{repeat}"),"turn_id":"bench","cwd":repo,"model":"benchmark","tool_name":"PowerShell","tool_use_id":format!("bench-{repeat}"),"tool_input":{"command":"benchmark command"}}).to_string().into_bytes()
}
fn run_in_process(
    repo: &Path,
    home: &Path,
    event: &str,
    repeat: usize,
) -> (Duration, prov_capture::TimingSnapshot) {
    std::env::set_var("PROV_HOME", home);
    let data = payload(repo, event, repeat);
    let start = Instant::now();
    let phases = prov_capture::process_json_timed(&data);
    (start.elapsed(), phases)
}
fn run_binary(binary: &Path, repo: &Path, home: &Path, event: &str, repeat: usize) -> Duration {
    let data = payload(repo, event, repeat);
    let start = Instant::now();
    let mut child = Command::new(binary)
        .arg("codex")
        .env("PROV_HOME", home)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(&data).unwrap();
    assert_eq!(child.wait().unwrap().code(), Some(0));
    start.elapsed()
}
fn complete(home: &Path) -> bool {
    let mut files = vec![home.to_path_buf()];
    while let Some(path) = files.pop() {
        let Ok(entries) = fs::read_dir(path) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                files.push(path)
            } else if path.extension().is_some_and(|x| x == "jsonl")
                && fs::read_to_string(&path).is_ok_and(|s| {
                    s.contains("scan_incomplete")
                        || (path.file_name().is_some_and(|x| x == "markers.jsonl")
                            && s.contains("time_cap"))
                })
            {
                return false;
            }
        }
    }
    true
}
fn summary(values: &[Duration]) -> String {
    let mut ms: Vec<f64> = values.iter().map(|d| d.as_secs_f64() * 1000.0).collect();
    ms.sort_by(f64::total_cmp);
    format!("{:.3} / {:.3}", ms[ms.len() / 2], ms[ms.len() - 1])
}
fn median_ms(values: &[Duration]) -> f64 {
    let mut v: Vec<f64> = values.iter().map(|d| d.as_secs_f64() * 1000.0).collect();
    v.sort_by(f64::total_cmp);
    v[v.len() / 2]
}
fn phase_median(
    values: &[prov_capture::TimingSnapshot],
    select: impl Fn(&prov_capture::TimingSnapshot) -> u64,
) -> f64 {
    let mut v: Vec<f64> = values
        .iter()
        .map(|p| select(p) as f64 / 1_000_000.0)
        .collect();
    v.sort_by(f64::total_cmp);
    v[v.len() / 2]
}
fn median_u64(
    values: &[prov_capture::TimingSnapshot],
    select: impl Fn(&prov_capture::TimingSnapshot) -> u64,
) -> u64 {
    let mut v: Vec<u64> = values.iter().map(select).collect();
    v.sort_unstable();
    v[v.len() / 2]
}
fn phase_remainder(values: &[prov_capture::TimingSnapshot]) -> f64 {
    let mut v: Vec<f64> = values
        .iter()
        .map(|p| {
            let accounted = p
                .repo_discovery_ns
                .saturating_add(p.lock_ns)
                .saturating_add(p.git_status_ns)
                .saturating_add(p.hashing_snapshots_ns)
                .saturating_add(p.writes_ns)
                .saturating_add(p.log_work_ns);
            p.total_ns.saturating_sub(accounted) as f64 / 1_000_000.0
        })
        .collect();
    v.sort_by(f64::total_cmp);
    v[v.len() / 2]
}

fn run_fsmonitor_experiment(repo: &Path, size: usize, base: &Path) {
    let args = [
        "-c",
        "core.quotepath=false",
        "-c",
        "core.fsmonitor=true",
        "-c",
        "core.untrackedCache=true",
        "status",
        "--porcelain=v2",
        "-z",
        "--untracked-files=all",
    ];
    let plain_args = [
        "-c",
        "core.quotepath=false",
        "status",
        "--porcelain=v2",
        "-z",
        "--untracked-files=all",
    ];
    let before = git_metadata(repo);
    let (mut first, first_ok, first_diag) = timed_status(repo, &args);
    let mut fallback_count = 0;
    if !first_ok {
        let (fallback, ok, _) = timed_status(repo, &plain_args);
        first += fallback;
        if ok {
            fallback_count += 1;
        }
    }
    let mut later = Vec::new();
    let mut later_ok = 0;
    for _ in 0..4 {
        let (mut d, ok, diag) = timed_status(repo, &args);
        if !ok {
            let (fallback, fallback_ok, _) = timed_status(repo, &plain_args);
            d += fallback;
            if fallback_ok {
                fallback_count += 1;
            }
        }
        later.push(d);
        if ok {
            later_ok += 1;
        }
        if !diag.is_empty() {
            println!("  fsmonitor diagnostic: {}", diag.replace('\n', " "));
        }
    }
    let after = git_metadata(repo);
    let status = Command::new("git")
        .args(["fsmonitor--daemon", "status"])
        .current_dir(repo)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output();
    let daemon = status.as_ref().is_ok_and(|o| o.status.success());
    let changed: Vec<String> = after
        .iter()
        .filter(|(p, v)| before.get(*p) != Some(*v))
        .map(|(p, _)| p.clone())
        .collect();
    let missing: Vec<String> = before
        .keys()
        .filter(|p| !after.contains_key(*p))
        .cloned()
        .collect();
    let mut later_copy = later.clone();
    later_copy.sort_by_key(Duration::as_nanos);
    println!("  fsmonitor experiment ({size} tracked; command-line config only): first {:.3} ms (success {first_ok}); later median/worst {} ms ({later_ok}/4 success); plain-status fallbacks {fallback_count}; daemon active {}; .git changed paths {}; removed paths {}; daemon-status {}", first.as_secs_f64()*1000.0, summary(&later_copy), daemon, changed.len(), missing.len(), status.as_ref().map(|o|String::from_utf8_lossy(&o.stdout).trim().replace('\n'," ")).unwrap_or_else(|e|format!("unavailable:{e}")));
    if !first_diag.is_empty() {
        println!("  fsmonitor diagnostic: {}", first_diag.replace('\n', " "));
    }
    if let Ok(output) = status {
        let err = String::from_utf8_lossy(&output.stderr);
        if !err.trim().is_empty() {
            println!("  fsmonitor diagnostic: {}", err.trim().replace('\n', " "));
        }
    }
    if !changed.is_empty() || !missing.is_empty() {
        println!(
            "  .git paths changed/removed: {} / {}",
            changed.join(","),
            missing.join(",")
        );
    }
    let _ = base;
}
fn timed_status(repo: &Path, args: &[&str]) -> (Duration, bool, String) {
    let start = Instant::now();
    let out = Command::new("git")
        .args(args)
        .current_dir(repo)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output();
    let elapsed = start.elapsed();
    match out {
        Ok(o) => (
            elapsed,
            o.status.success(),
            String::from_utf8_lossy(&o.stderr).into_owned(),
        ),
        Err(e) => (elapsed, false, e.to_string()),
    }
}
fn git_metadata(repo: &Path) -> std::collections::BTreeMap<String, (u64, u128)> {
    let mut out = std::collections::BTreeMap::new();
    let git = repo.join(".git");
    let mut todo = vec![git.clone()];
    while let Some(dir) = todo.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                todo.push(p)
            } else if let Ok(m) = fs::metadata(&p) {
                let key = p
                    .strip_prefix(&git)
                    .unwrap_or(&p)
                    .to_string_lossy()
                    .replace('\\', "/");
                let mt = m
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map_or(0, |d| d.as_nanos());
                out.insert(key, (m.len(), mt));
            }
        }
    }
    out
}

fn run_git_ai_baseline(repo: &Path, tracked: usize) {
    let path = if tracked == 0 {
        "README.txt"
    } else {
        "tracked-00000.txt"
    };
    let baseline = if tracked == 0 {
        "untracked benchmark baseline\n".to_owned()
    } else {
        "original 0\nline two\n".to_owned()
    };
    let mut durations = Vec::new();
    for n in 0..REPEATS {
        fs::write(repo.join(path), &baseline).unwrap();
        fs::write(repo.join(path), format!("checkpoint baseline {n}\n")).unwrap();
        let start = Instant::now();
        let out = Command::new(GIT_AI)
            .args(["checkpoint", "mock_ai", path])
            .current_dir(repo)
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_TERMINAL_PROMPT", "0")
            .output();
        let elapsed = start.elapsed();
        match out {
            Ok(o) if o.status.success() => durations.push(elapsed),
            Ok(o) => {
                println!(
                    "  Git AI baseline ({tracked} tracked): skipped, exit {}; {}",
                    o.status,
                    String::from_utf8_lossy(&o.stderr).trim()
                );
                let _ = fs::write(repo.join(path), baseline);
                return;
            }
            Err(e) => {
                println!("  Git AI baseline ({tracked} tracked): skipped, {e}");
                let _ = fs::write(repo.join(path), baseline);
                return;
            }
        }
        fs::write(repo.join(path), &baseline).unwrap();
    }
    println!("  Git AI checkpoint mock_ai, one changed file ({tracked} tracked): {} ms over {REPEATS} local runs",summary(&durations));
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
    assert!(status.success(), "git {args:?} failed");
}
fn now() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}
