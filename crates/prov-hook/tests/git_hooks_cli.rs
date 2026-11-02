use std::fs;
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
