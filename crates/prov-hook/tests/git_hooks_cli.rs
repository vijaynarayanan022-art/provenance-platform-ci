use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Mutex, MutexGuard};

static TEST_GATE: Mutex<()> = Mutex::new(());

fn test_guard() -> MutexGuard<'static, ()> {
    TEST_GATE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn isolated_command(program: impl AsRef<std::ffi::OsStr>) -> Command {
    let null = if cfg!(windows) { "NUL" } else { "/dev/null" };
    let mut command = Command::new(program);
    command
        .env("GIT_CONFIG_GLOBAL", null)
        .env("GIT_CONFIG_NOSYSTEM", "1");
    command
}

struct Fixture {
    root: PathBuf,
    repo: PathBuf,
    store: PathBuf,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "prov-git-hooks-{name}-{}-{}",
            std::process::id(),
            SystemTimeNow::now()
        ));
        let repo = root.join("repo");
        let store = root.join("prov-home");
        fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init"]);
        git(&repo, &["config", "user.name", "Test User"]);
        git(&repo, &["config", "user.email", "test@example.invalid"]);
        fs::write(repo.join("tracked.txt"), "hello\n").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-m", "init"]);
        Self { root, repo, store }
    }

    fn cli(&self, args: &[&str]) -> Output {
        isolated_command(env!("CARGO_BIN_EXE_prov-hook"))
            .args(args)
            .env("PROV_HOME", &self.store)
            .output()
            .unwrap()
    }

    fn install_git(&self) -> Output {
        self.cli(&["install-git", "--repo", self.repo.to_str().unwrap()])
    }

    fn uninstall_git(&self) -> Output {
        self.cli(&["uninstall-git", "--repo", self.repo.to_str().unwrap()])
    }

    fn effective_hooks_dir(&self) -> PathBuf {
        git_path(&self.repo, &["rev-parse", "--git-path", "hooks"])
    }

    fn drop_root(&self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

struct SystemTimeNow;

impl SystemTimeNow {
    fn now() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    }
}

fn git(repo: &Path, args: &[&str]) {
    let output = isolated_command("git")
        .args(args)
        .current_dir(repo)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(output.success(), "git {:?} failed for {:?}", args, repo);
}

fn git_path(repo: &Path, args: &[&str]) -> PathBuf {
    let output = isolated_command("git")
        .args(args)
        .current_dir(repo)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .unwrap();
    assert!(output.status.success());
    let value = String::from_utf8(output.stdout).unwrap();
    let trimmed = value.trim();
    if trimmed.is_empty() {
        repo.to_path_buf()
    } else {
        let path = PathBuf::from(trimmed);
        if path.is_absolute() {
            path
        } else {
            repo.join(path)
        }
    }
}

#[test]
fn install_git_creates_hooks_and_is_idempotent() {
    let _guard = test_guard();
    let fixture = Fixture::new("install-idempotent");
    let output = fixture.install_git();
    assert!(output.status.success(), "{:?}", output);
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("post-commit: installed"));
    assert!(text.contains("post-rewrite: installed"));

    let hooks_dir = fixture.effective_hooks_dir();
    let commit_hook = hooks_dir.join("post-commit");
    let rewrite_hook = hooks_dir.join("post-rewrite");
    assert!(commit_hook.exists());
    assert!(rewrite_hook.exists());
    let commit_text = fs::read_to_string(&commit_hook).unwrap();
    let rewrite_text = fs::read_to_string(&rewrite_hook).unwrap();
    assert!(!commit_text.contains("--repo"));
    assert!(!rewrite_text.contains("--repo"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = fs::metadata(&commit_hook).unwrap().permissions();
        assert!((perms.mode() & 0o111) != 0);
    }

    let second = fixture.install_git();
    assert!(second.status.success());
    let first_bytes = fs::read(&commit_hook).unwrap();
    let second_bytes = fs::read(&rewrite_hook).unwrap();
    assert_eq!(fs::read(&commit_hook).unwrap(), first_bytes);
    assert_eq!(fs::read(&rewrite_hook).unwrap(), second_bytes);
    fixture.drop_root();
}

#[test]
fn install_git_upgrades_old_repo_argument_inside_only_our_block() {
    let _guard = test_guard();
    let fixture = Fixture::new("upgrade-old-block");
    let hook = fixture.effective_hooks_dir().join("post-commit");
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_prov-hook"))
        .to_string_lossy()
        .replace('\\', "/");
    let repo = fixture.repo.to_string_lossy().replace('\\', "/");
    let old = format!(
        "#!/bin/sh\n# before\n# >>> prov-hook >>>\n[ -x '{binary}' ] && '{binary}' git-event post-commit --repo '{repo}' \"$@\" </dev/null >/dev/null 2>&1 &\n# <<< prov-hook <<<\n# after\n"
    );
    fs::write(&hook, &old).unwrap();
    let installed = fixture.install_git();
    assert!(installed.status.success());
    let updated = fs::read_to_string(&hook).unwrap();
    assert!(updated.starts_with("#!/bin/sh\n# before\n"));
    assert!(updated.ends_with("# after\n"));
    assert!(!updated.contains("--repo"));
    assert_eq!(updated.matches("# >>> prov-hook >>>").count(), 1);
    fixture.drop_root();
}

#[test]
fn install_git_uses_effective_hooks_dir_and_preserves_sh_hook_bytes() {
    let _guard = test_guard();
    let fixture = Fixture::new("hooks-dir");
    let custom = fixture.repo.join("custom-hooks");
    fs::create_dir_all(&custom).unwrap();
    git(&fixture.repo, &["config", "core.hooksPath", "custom-hooks"]);

    let installed = fixture.install_git();
    assert!(installed.status.success());
    let custom_commit = custom.join("post-commit");
    let custom_rewrite = custom.join("post-rewrite");
    assert!(custom_commit.exists());
    assert!(custom_rewrite.exists());

    let hook_path = custom_commit.clone();
    let original = "#!/bin/sh\nset -eu\nprintf 'before\\n'\n";
    fs::write(&hook_path, original).unwrap();
    let before = fs::read(&hook_path).unwrap();
    let install = fixture.install_git();
    assert!(install.status.success());
    let after = fs::read(&hook_path).unwrap();
    assert!(after.starts_with(b"#!/bin/sh\n"));
    assert!(after.len() > before.len());
    assert!(String::from_utf8(after.clone())
        .unwrap()
        .contains("# >>> prov-hook >>>"));

    let again = fixture.uninstall_git();
    assert!(again.status.success());
    assert_eq!(fs::read(&hook_path).unwrap(), before);
    fixture.drop_root();
}

#[test]
fn unsupported_non_sh_hook_is_untouched() {
    let _guard = test_guard();
    let fixture = Fixture::new("non-sh");
    let hook = fixture.effective_hooks_dir().join("post-commit");
    let original = "#!/usr/bin/env python3\nprint('leave me alone')\n";
    fs::write(&hook, original).unwrap();

    let output = fixture.install_git();
    assert!(output.status.success());
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("unsupported existing hook"));
    assert_eq!(fs::read(&hook).unwrap(), original.as_bytes());
    fixture.drop_root();
}

#[test]
fn doctor_reports_git_hooks_status_without_writing() {
    let _guard = test_guard();
    let fixture = Fixture::new("doctor");
    let doctor = isolated_command(env!("CARGO_BIN_EXE_prov-hook"))
        .args(["doctor", "--repo", fixture.repo.to_str().unwrap()])
        .env("PROV_HOME", &fixture.store)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&doctor.stdout);
    assert!(text.contains("git hooks"));
    assert_eq!(doctor.stderr, b"");
    fixture.drop_root();
}

#[test]
fn git_event_post_commit_is_noop_without_store() {
    let _guard = test_guard();
    let fixture = Fixture::new("no-store");
    let store = fixture.root.join("missing-store");
    let output = isolated_command(env!("CARGO_BIN_EXE_prov-hook"))
        .args([
            "git-event",
            "post-commit",
            "--repo",
            fixture.repo.to_str().unwrap(),
        ])
        .env("PROV_HOME", &store)
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(!store.exists());
    fixture.drop_root();
}

fn seed_capture_history(fixture: &Fixture) -> String {
    let payload = serde_json::json!({
        "hook_event_name": "Stop",
        "session_id": "git-hook-seed",
        "cwd": fixture.repo,
        "tool_use_id": "git-hook-seed-tool",
    });
    let mut child = isolated_command(env!("CARGO_BIN_EXE_prov-hook"))
        .arg("codex")
        .current_dir(&fixture.repo)
        .env("PROV_HOME", &fixture.store)
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
    fs::read_dir(fixture.store.join("repos"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .file_name()
        .to_string_lossy()
        .into_owned()
}

#[test]
fn git_event_without_repo_resolves_from_a_repository_subdirectory() {
    let _guard = test_guard();
    let fixture = Fixture::new("repo-less-subdir");
    let repo_id = seed_capture_history(&fixture);
    let subdir = fixture.repo.join("nested");
    fs::create_dir_all(&subdir).unwrap();
    let output = isolated_command(env!("CARGO_BIN_EXE_prov-hook"))
        .args(["git-event", "post-commit"])
        .current_dir(&subdir)
        .env("PROV_HOME", &fixture.store)
        .env("GIT_DIR", fixture.repo.join(".git"))
        .env("GIT_WORK_TREE", &fixture.repo)
        .output()
        .unwrap();
    assert!(output.status.success());
    let queued = fs::read_to_string(
        fixture
            .store
            .join("repos")
            .join(repo_id)
            .join("events")
            .join("git.jsonl"),
    )
    .unwrap();
    assert!(queued.contains("\"event\":\"head_move\""));
    fixture.drop_root();
}

#[test]
fn plain_git_commit_through_installed_hook_derives_the_commit() {
    let _guard = test_guard();
    let fixture = Fixture::new("plain-commit-derives");
    seed_capture_history(&fixture);
    let installed = fixture.install_git();
    assert!(installed.status.success());

    fs::write(fixture.repo.join("tracked.txt"), "changed by plain git\n").unwrap();
    git(&fixture.repo, &["add", "tracked.txt"]);
    let commit = isolated_command("git")
        .args(["commit", "-m", "plain git commit"])
        .current_dir(&fixture.repo)
        .env("PROV_HOME", &fixture.store)
        .env_remove("PROV_NO_AUTODERIVE")
        .output()
        .unwrap();
    assert!(
        commit.status.success(),
        "{}",
        String::from_utf8_lossy(&commit.stderr)
    );
    let head_output = isolated_command("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(&fixture.repo)
        .output()
        .unwrap();
    assert!(head_output.status.success());
    let head = String::from_utf8(head_output.stdout)
        .unwrap()
        .trim()
        .to_owned();
    let mut derived = false;
    for _ in 0..80 {
        let show = isolated_command(env!("CARGO_BIN_EXE_prov-hook"))
            .args([
                "show",
                "--repo",
                fixture.repo.to_str().unwrap(),
                "--commit",
                &head,
                "--json",
            ])
            .env("PROV_HOME", &fixture.store)
            .output()
            .unwrap();
        if show.status.success() {
            derived = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(
        derived,
        "plain git post-commit hook did not derive HEAD {head}"
    );
    fixture.drop_root();
}

#[test]
fn post_rewrite_event_queues_new_commits_when_repo_has_store() {
    let _guard = test_guard();
    let fixture = Fixture::new("rewrite-store");
    let store = fixture.root.join("prov-home");
    let repo_store = store.join("repos");
    fs::create_dir_all(&repo_store).unwrap();
    let repo_id = "test-repo";
    fs::create_dir_all(repo_store.join(repo_id).join("events")).unwrap();
    let output = isolated_command(env!("CARGO_BIN_EXE_prov-hook"))
        .args([
            "git-event",
            "post-rewrite",
            "--repo",
            fixture.repo.to_str().unwrap(),
        ])
        .env("PROV_HOME", &store)
        .stdin(Stdio::piped())
        .output()
        .unwrap();
    assert!(output.status.success());
    fixture.drop_root();
}
