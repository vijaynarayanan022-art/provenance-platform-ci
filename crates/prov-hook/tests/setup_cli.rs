use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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
    codex_home: PathBuf,
    store: PathBuf,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "prov-setup-{name}-{}-{}",
            std::process::id(),
            now()
        ));
        let codex_home = root.join("Codex Home with spaces");
        let store = root.join("external provenance store");
        fs::create_dir_all(&root).unwrap();
        Self {
            root,
            codex_home,
            store,
        }
    }

    fn cli(&self, args: &[&str]) -> Output {
        let local_app_data = self.local_app_data();
        isolated_command(env!("CARGO_BIN_EXE_prov-hook"))
            .args(args)
            .env("PROV_HOME", &self.store)
            .env("LOCALAPPDATA", local_app_data)
            .output()
            .unwrap()
    }

    fn local_app_data(&self) -> PathBuf {
        self.root.join("Local AppData")
    }

    fn home_arg(&self) -> &str {
        self.codex_home.to_str().unwrap()
    }

    fn config(&self) -> PathBuf {
        self.codex_home.join("config.toml")
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

fn backups(config: &Path) -> Vec<PathBuf> {
    let prefix = format!("{}.prov-backup-", config.display());
    let Some(parent) = config.parent().filter(|parent| parent.exists()) else {
        return Vec::new();
    };
    let mut found: Vec<_> = fs::read_dir(parent)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.to_string_lossy().starts_with(&prefix))
        .collect();
    found.sort();
    found
}

fn snapshot(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut result = BTreeMap::new();
    if !root.exists() {
        return result;
    }
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(directory).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else {
                result.insert(
                    path.strip_prefix(root).unwrap().to_path_buf(),
                    fs::read(path).unwrap(),
                );
            }
        }
    }
    result
}

fn git(repo: &Path, args: &[&str]) {
    let output = isolated_command("git")
        .args(args)
        .current_dir(repo)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_AUTHOR_NAME", "Setup Test")
        .env("GIT_AUTHOR_EMAIL", "setup@example.invalid")
        .env("GIT_COMMITTER_NAME", "Setup Test")
        .env("GIT_COMMITTER_EMAIL", "setup@example.invalid")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(output.success(), "git {args:?} failed");
}

#[test]
fn isolated_git_does_not_create_notes_after_commit() {
    let fixture = Fixture::new("no-global-git-ai");
    let repo = fixture.root.join("isolated repository");
    fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init"]);
    fs::write(repo.join("tracked.txt"), b"content\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "initial"]);

    std::thread::sleep(Duration::from_millis(500));
    assert!(!repo.join(".git/refs/notes").exists());
    fixture.drop_root();
}

#[test]
fn install_creates_missing_config_and_is_idempotent() {
    let fixture = Fixture::new("missing");
    let dry_run = fixture.cli(&["install", "--codex-home", fixture.home_arg(), "--dry-run"]);
    assert!(dry_run.status.success());
    assert!(String::from_utf8_lossy(&dry_run.stdout).contains("+++"));
    assert!(!fixture.config().exists());
    assert!(backups(&fixture.config()).is_empty());

    let installed = fixture.cli(&[
        "install",
        "--codex-home",
        fixture.home_arg(),
        "--enable-hooks-feature",
    ]);
    assert!(
        installed.status.success(),
        "{}",
        String::from_utf8_lossy(&installed.stderr)
    );
    let first = fs::read(fixture.config()).unwrap();
    assert_eq!(backups(&fixture.config()).len(), 1);
    assert!(fs::read(&backups(&fixture.config())[0]).unwrap().is_empty());
    let text = String::from_utf8(first.clone()).unwrap();
    assert!(text.contains("command = '") && text.contains(" codex'"));

    let repeated = fixture.cli(&["install", "--codex-home", fixture.home_arg()]);
    assert!(repeated.status.success());
    assert_eq!(fs::read(fixture.config()).unwrap(), first);
    assert_eq!(backups(&fixture.config()).len(), 1);
    let text = String::from_utf8(first).unwrap();
    for event in ["PreToolUse", "PostToolUse", "Stop"] {
        assert_eq!(text.matches(&format!("[[hooks.{event}]]")).count(), 1);
    }
    fixture.drop_root();
}

#[test]
fn dry_run_uses_unified_diff_with_local_context() {
    let fixture = Fixture::new("unified-diff");
    fs::create_dir_all(&fixture.codex_home).unwrap();
    let mut original = String::from("model = \"unchanged\"\n");
    for index in 0..20 {
        original.push_str(&format!("unchanged-{index:02} = true\n"));
    }
    fs::write(fixture.config(), &original).unwrap();

    let result = fixture.cli(&["install", "--codex-home", fixture.home_arg(), "--dry-run"]);
    assert!(result.status.success());
    let diff = String::from_utf8_lossy(&result.stdout);
    assert!(diff.contains("@@"));
    assert!(diff.contains(" unchanged-17 = true"));
    assert!(!diff.contains("-unchanged-00 = true"));
    assert_eq!(fs::read(fixture.config()).unwrap(), original.as_bytes());
    assert!(backups(&fixture.config()).is_empty());
    fixture.drop_root();
}

#[test]
fn install_preserves_unrelated_config_and_quotes_spaced_binary() {
    let fixture = Fixture::new("existing");
    fs::create_dir_all(&fixture.codex_home).unwrap();
    let original = "model = \"unchanged\"\n[other]\nkey = true\n";
    fs::write(fixture.config(), original).unwrap();
    #[cfg(windows)]
    let binary = fixture.root.join("Program Files").join("prov hook.exe");
    #[cfg(not(windows))]
    let binary = fixture.root.join("Program Files").join("prov hook");
    let result = fixture.cli(&[
        "install",
        "--codex-home",
        fixture.home_arg(),
        "--binary",
        binary.to_str().unwrap(),
        "--enable-hooks-feature",
    ]);
    assert!(result.status.success());
    assert!(String::from_utf8_lossy(&result.stdout).contains(
        "WARNING: executable path contains spaces; verify with doctor and a real Codex session."
    ));
    let installed = fs::read_to_string(fixture.config()).unwrap();
    assert!(installed.starts_with(original));
    #[cfg(windows)]
    assert!(installed.contains(r#"Program Files\\prov hook.exe\" codex"#));
    #[cfg(not(windows))]
    assert!(installed.contains(&format!(r#"\"{}\" codex"#, binary.display())));
    assert_eq!(installed.matches("# >>> prov-hook >>>").count(), 1);
    assert_eq!(installed.matches("# <<< prov-hook <<<").count(), 1);
    assert_eq!(
        fs::read(&backups(&fixture.config())[0]).unwrap(),
        original.as_bytes()
    );
    assert!(!fs::read_dir(&fixture.codex_home)
        .unwrap()
        .flatten()
        .any(|entry| entry
            .file_name()
            .to_string_lossy()
            .starts_with(".config.toml.prov-")));

    let before_dry_run = fs::read(fixture.config()).unwrap();
    let backup_count = backups(&fixture.config()).len();
    let dry_run = fixture.cli(&["uninstall", "--codex-home", fixture.home_arg(), "--dry-run"]);
    assert!(dry_run.status.success());
    assert!(String::from_utf8_lossy(&dry_run.stdout).contains("---"));
    assert_eq!(fs::read(fixture.config()).unwrap(), before_dry_run);
    assert_eq!(backups(&fixture.config()).len(), backup_count);
    fixture.drop_root();
}

#[test]
fn uninstall_removes_only_the_marked_block_and_backs_up_first() {
    let fixture = Fixture::new("uninstall");
    fs::create_dir_all(&fixture.codex_home).unwrap();
    let original = "model = \"preserve me\"\n\n[projects]\ntrust = \"trusted\"\n";
    fs::write(fixture.config(), original).unwrap();
    assert!(fixture
        .cli(&[
            "install",
            "--codex-home",
            fixture.home_arg(),
            "--enable-hooks-feature",
        ])
        .status
        .success());
    let installed = fs::read(fixture.config()).unwrap();

    let removed = fixture.cli(&["uninstall", "--codex-home", fixture.home_arg()]);
    assert!(removed.status.success());
    assert_eq!(fs::read(fixture.config()).unwrap(), original.as_bytes());
    assert_eq!(backups(&fixture.config()).len(), 2);
    assert!(backups(&fixture.config())
        .iter()
        .any(|backup| fs::read(backup).unwrap() == installed));
    fixture.drop_root();
}

#[test]
fn install_refuses_conflicting_hook_syntax_without_changes() {
    for (name, conflict) in [
        ("event-assignment", "[hooks]\nPreToolUse = []\n"),
        ("inline-table", "[other]\nhooks = { Stop = [] }\n"),
    ] {
        let fixture = Fixture::new(name);
        fs::create_dir_all(&fixture.codex_home).unwrap();
        fs::write(fixture.config(), conflict).unwrap();
        let result = fixture.cli(&["install", "--codex-home", fixture.home_arg()]);
        assert!(!result.status.success());
        assert!(result.stdout.is_empty());
        assert!(String::from_utf8_lossy(&result.stderr).contains("refusing to install"));
        assert_eq!(fs::read(fixture.config()).unwrap(), conflict.as_bytes());
        assert!(backups(&fixture.config()).is_empty());
        fixture.drop_root();
    }
}

#[test]
fn feature_opt_in_creates_inside_block_or_reversibly_edits_existing_table() {
    let no_table = Fixture::new("feature-create");
    let without_flag = no_table.cli(&["install", "--codex-home", no_table.home_arg()]);
    assert!(without_flag.status.success());
    assert!(String::from_utf8_lossy(&without_flag.stdout).contains("NOTE:"));
    assert!(!String::from_utf8_lossy(&fs::read(no_table.config()).unwrap()).contains("[features]"));
    assert!(!no_table
        .cli(&["doctor", "--codex-home", no_table.home_arg()])
        .status
        .success());
    no_table.drop_root();

    let missing_key = Fixture::new("feature-insert");
    fs::create_dir_all(&missing_key.codex_home).unwrap();
    let original = "model = \"preserve\"\n[features]\nother = true\n";
    fs::write(missing_key.config(), original).unwrap();
    let install = missing_key.cli(&[
        "install",
        "--codex-home",
        missing_key.home_arg(),
        "--enable-hooks-feature",
    ]);
    assert!(install.status.success());
    let contents = fs::read_to_string(missing_key.config()).unwrap();
    assert!(contents.contains("[features]\nhooks = true # prov-hook\nother = true"));
    assert!(missing_key
        .cli(&["doctor", "--codex-home", missing_key.home_arg()])
        .status
        .success());
    assert!(missing_key
        .cli(&["uninstall", "--codex-home", missing_key.home_arg()])
        .status
        .success());
    assert_eq!(fs::read(missing_key.config()).unwrap(), original.as_bytes());
    missing_key.drop_root();
}

#[test]
fn feature_false_is_marked_and_uninstall_restores_exact_original_line() {
    let fixture = Fixture::new("feature-false");
    fs::create_dir_all(&fixture.codex_home).unwrap();
    let original = "[features]\nhooks = false # retain this comment\n\n[other]\nkey = true\n";
    fs::write(fixture.config(), original).unwrap();
    let install = fixture.cli(&[
        "install",
        "--codex-home",
        fixture.home_arg(),
        "--enable-hooks-feature",
    ]);
    assert!(install.status.success());
    let contents = fs::read_to_string(fixture.config()).unwrap();
    assert!(contents.contains("hooks = true # prov-hook (was false)"));
    assert!(fixture
        .cli(&["uninstall", "--codex-home", fixture.home_arg()])
        .status
        .success());
    assert_eq!(fs::read(fixture.config()).unwrap(), original.as_bytes());
    fixture.drop_root();
}

#[test]
fn doctor_reports_install_and_empty_config_without_writing() {
    let fixture = Fixture::new("doctor");
    fs::create_dir_all(&fixture.codex_home).unwrap();
    fs::write(fixture.config(), "").unwrap();
    let before_empty = snapshot(&fixture.root);
    let empty = fixture.cli(&["doctor", "--codex-home", fixture.home_arg()]);
    assert!(!empty.status.success());
    assert!(String::from_utf8_lossy(&empty.stdout).contains("FAIL managed block"));
    assert_eq!(snapshot(&fixture.root), before_empty);

    let installed = fixture.cli(&[
        "install",
        "--codex-home",
        fixture.home_arg(),
        "--enable-hooks-feature",
    ]);
    assert!(installed.status.success());
    let before_doctor = snapshot(&fixture.root);
    let doctor = fixture.cli(&["doctor", "--codex-home", fixture.home_arg()]);
    assert!(
        doctor.status.success(),
        "{}",
        String::from_utf8_lossy(&doctor.stdout)
    );
    let output = String::from_utf8_lossy(&doctor.stdout);
    assert!(output.contains("PASS managed block"));
    assert!(output.contains("PASS hook entry: PreToolUse"));
    assert!(output.contains("PASS hook entry: PostToolUse"));
    assert!(output.contains("PASS hook entry: Stop"));
    assert!(output.contains("WARN hook state/trust: open Codex and approve hooks"));
    assert_eq!(snapshot(&fixture.root), before_doctor);
    fixture.drop_root();
}

#[test]
fn doctor_checks_the_managed_executable_and_warns_when_it_differs() {
    let fixture = Fixture::new("doctor-binary");
    let binary = fixture.root.join("alternate prov hook.exe");
    fs::copy(env!("CARGO_BIN_EXE_prov-hook"), &binary).unwrap();
    let installed = fixture.cli(&[
        "install",
        "--codex-home",
        fixture.home_arg(),
        "--binary",
        binary.to_str().unwrap(),
        "--enable-hooks-feature",
    ]);
    assert!(installed.status.success());

    let doctor = fixture.cli(&["doctor", "--codex-home", fixture.home_arg()]);
    assert!(doctor.status.success());
    let output = String::from_utf8_lossy(&doctor.stdout);
    assert!(output.contains("WARN binary: configured executable differs from running binary"));
    assert!(output.contains("PASS managed block"));
    fixture.drop_root();
}

#[test]
fn doctor_resolves_trust_state_by_event_group_and_accepts_default_enabled() {
    let fixture = Fixture::new("doctor-trust-groups");
    fs::create_dir_all(&fixture.codex_home).unwrap();
    fs::write(
        fixture.config(),
        "[[hooks.PreToolUse]]\n[[hooks.Stop]]\nmodel = \"unrelated\"\n",
    )
    .unwrap();
    assert!(fixture
        .cli(&[
            "install",
            "--codex-home",
            fixture.home_arg(),
            "--enable-hooks-feature",
        ])
        .status
        .success());

    let config_key = fixture.config().display().to_string();
    let pre_key = format!("{config_key}:pre_tool_use:1:0");
    let post_key = format!("{config_key}:post_tool_use:0:0");
    let stop_key = format!("{config_key}:stop:1:0");
    let state = format!(
        "\n[hooks.state.'{pre_key}']\ntrusted_hash = \"sha256:fixture\"\n\n[hooks.state.\"{}\"]\ntrusted_hash = \"sha256:fixture\"\n\n[hooks.state.'{stop_key}']\ntrusted_hash = \"sha256:fixture\"\n",
        post_key.replace('\\', "\\\\")
    );
    use std::fs::OpenOptions;
    OpenOptions::new()
        .append(true)
        .open(fixture.config())
        .unwrap()
        .write_all(state.as_bytes())
        .unwrap();

    let doctor = fixture.cli(&["doctor", "--codex-home", fixture.home_arg()]);
    assert!(doctor.status.success());
    assert!(String::from_utf8_lossy(&doctor.stdout)
        .contains("PASS hook state/trust: all three entries are trusted and not disabled"));
    fixture.drop_root();
}

#[test]
fn doctor_warns_when_a_hook_state_entry_is_explicitly_disabled() {
    let fixture = Fixture::new("doctor-trust-disabled");
    fs::create_dir_all(&fixture.codex_home).unwrap();
    assert!(fixture
        .cli(&[
            "install",
            "--codex-home",
            fixture.home_arg(),
            "--enable-hooks-feature",
        ])
        .status
        .success());
    let state = format!(
        "\n[hooks.state.'{}:pre_tool_use:0:0']\ntrusted_hash = \"sha256:fixture\"\nenabled = false\n",
        fixture.config().display()
    );
    use std::fs::OpenOptions;
    OpenOptions::new()
        .append(true)
        .open(fixture.config())
        .unwrap()
        .write_all(state.as_bytes())
        .unwrap();
    let doctor = fixture.cli(&["doctor", "--codex-home", fixture.home_arg()]);
    assert!(doctor.status.success());
    assert!(String::from_utf8_lossy(&doctor.stdout)
        .contains("WARN hook state/trust: open Codex and approve hooks"));
    fixture.drop_root();
}

#[test]
fn doctor_reports_repo_identity_events_and_last_event_without_writing() {
    let fixture = Fixture::new("doctor-repo");
    let repo = fixture.root.join("work repository");
    fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init"]);
    fs::write(repo.join("tracked.txt"), b"content\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "initial"]);

    let payload = serde_json::json!({
        "hook_event_name": "Stop",
        "session_id": "doctor-repo-session",
        "cwd": repo,
        "tool_use_id": "doctor-repo-tool",
    });
    let mut child = isolated_command(env!("CARGO_BIN_EXE_prov-hook"))
        .arg("codex")
        .env("PROV_HOME", &fixture.store)
        .env("PROV_NO_AUTODERIVE", "1")
        .env("LOCALAPPDATA", fixture.local_app_data())
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
    let hook = child.wait_with_output().unwrap();
    assert_eq!(hook.status.code(), Some(0));
    assert!(hook.stdout.is_empty() && hook.stderr.is_empty());

    fs::create_dir_all(&fixture.codex_home).unwrap();
    assert!(fixture
        .cli(&[
            "install",
            "--codex-home",
            fixture.home_arg(),
            "--enable-hooks-feature",
        ])
        .status
        .success());
    let before = snapshot(&fixture.root);
    let doctor = fixture.cli(&[
        "doctor",
        "--codex-home",
        fixture.home_arg(),
        "--repo",
        repo.to_str().unwrap(),
    ]);
    assert!(doctor.status.success());
    let output = String::from_utf8_lossy(&doctor.stdout);
    assert!(output.contains("PASS repo identity:"));
    assert!(output.contains("PASS repo events: 1 event log(s) found"));
    assert!(output.contains("PASS last event time:"));
    assert!(output.contains("PASS pending commits: 0 not yet derived"));
    assert_eq!(snapshot(&fixture.root), before);

    let repo_id = fs::read_dir(fixture.store.join("repos"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .file_name()
        .to_string_lossy()
        .into_owned();
    let head = isolated_command("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(&repo)
        .output()
        .unwrap();
    let head = String::from_utf8(head.stdout).unwrap().trim().to_owned();
    let old_timestamp = now().saturating_sub(6 * 60 * 1_000_000_000);
    writeln!(
        fs::OpenOptions::new()
            .append(true)
            .open(
                fixture
                    .store
                    .join("repos")
                    .join(repo_id)
                    .join("events")
                    .join("doctor-repo-session.jsonl"),
            )
            .unwrap(),
        "{}",
        serde_json::json!({"event":"head_move", "timestamp":old_timestamp, "new_head":head})
    )
    .unwrap();
    let before_pending_doctor = snapshot(&fixture.root);
    let pending_doctor = fixture.cli(&[
        "doctor",
        "--codex-home",
        fixture.home_arg(),
        "--repo",
        repo.to_str().unwrap(),
    ]);
    assert!(pending_doctor.status.success());
    assert!(String::from_utf8_lossy(&pending_doctor.stdout)
        .contains("WARN pending commits: 1 not yet derived"));
    assert_eq!(snapshot(&fixture.root), before_pending_doctor);
    fixture.drop_root();
}

#[test]
fn doctor_warns_when_repo_events_are_only_in_default_store() {
    let fixture = Fixture::new("doctor-default-store");
    let repo = fixture.root.join("default-store repository");
    fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init"]);
    fs::write(repo.join("tracked.txt"), b"content\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "initial"]);

    let payload = serde_json::json!({
        "hook_event_name": "Stop",
        "session_id": "doctor-default-session",
        "cwd": repo,
        "tool_use_id": "doctor-default-tool",
    });
    let mut child = isolated_command(env!("CARGO_BIN_EXE_prov-hook"))
        .arg("codex")
        .env_remove("PROV_HOME")
        .env("LOCALAPPDATA", fixture.local_app_data())
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
    assert!(child.wait_with_output().unwrap().status.success());

    fs::create_dir_all(&fixture.codex_home).unwrap();
    assert!(fixture
        .cli(&[
            "install",
            "--codex-home",
            fixture.home_arg(),
            "--enable-hooks-feature",
        ])
        .status
        .success());
    let before = snapshot(&fixture.root);
    let doctor = fixture.cli(&[
        "doctor",
        "--codex-home",
        fixture.home_arg(),
        "--repo",
        repo.to_str().unwrap(),
    ]);
    assert!(doctor.status.success());
    let output = String::from_utf8_lossy(&doctor.stdout);
    assert!(output.contains("PASS repo identity:"));
    assert!(output.contains("WARN repo events: none found in resolved store"));
    assert!(output.contains("WARN repo events: 1 event log(s) found in default store"));
    assert!(output.contains("WARN PROV_HOME differs from default store"));
    assert_eq!(snapshot(&fixture.root), before);
    fixture.drop_root();
}
