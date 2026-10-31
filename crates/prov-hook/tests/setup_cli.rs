use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

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
        Command::new(env!("CARGO_BIN_EXE_prov-hook"))
            .args(args)
            .env("PROV_HOME", &self.store)
            .output()
            .unwrap()
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
    let output = Command::new("git")
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
fn install_creates_missing_config_and_is_idempotent() {
    let fixture = Fixture::new("missing");
    let dry_run = fixture.cli(&["install", "--codex-home", fixture.home_arg(), "--dry-run"]);
    assert!(dry_run.status.success());
    assert!(String::from_utf8_lossy(&dry_run.stdout).contains("+++"));
    assert!(!fixture.config().exists());
    assert!(backups(&fixture.config()).is_empty());

    let installed = fixture.cli(&["install", "--codex-home", fixture.home_arg()]);
    assert!(
        installed.status.success(),
        "{}",
        String::from_utf8_lossy(&installed.stderr)
    );
    let first = fs::read(fixture.config()).unwrap();
    assert_eq!(backups(&fixture.config()).len(), 1);
    assert!(fs::read(&backups(&fixture.config())[0]).unwrap().is_empty());

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
fn install_preserves_unrelated_config_and_quotes_spaced_windows_binary() {
    let fixture = Fixture::new("existing");
    fs::create_dir_all(&fixture.codex_home).unwrap();
    let original = "model = \"unchanged\"\n[other]\nkey = true\n";
    fs::write(fixture.config(), original).unwrap();
    let binary = fixture.root.join("Program Files").join("prov hook.exe");
    let result = fixture.cli(&[
        "install",
        "--codex-home",
        fixture.home_arg(),
        "--binary",
        binary.to_str().unwrap(),
    ]);
    assert!(result.status.success());
    let installed = fs::read_to_string(fixture.config()).unwrap();
    assert!(installed.starts_with(original));
    assert!(installed.contains(r#"Program Files\\prov hook.exe\" codex"#));
    assert_eq!(installed.matches("# >>> prov-hook >>>").count(), 1);
    assert_eq!(installed.matches("# <<< prov-hook <<<").count(), 1);
    assert_eq!(
        fs::read(&backups(&fixture.config())[0]).unwrap(),
        original.as_bytes()
    );

    let before_dry_run = fs::read(&fixture.config()).unwrap();
    let backup_count = backups(&fixture.config()).len();
    let dry_run = fixture.cli(&["uninstall", "--codex-home", fixture.home_arg(), "--dry-run"]);
    assert!(dry_run.status.success());
    assert!(String::from_utf8_lossy(&dry_run.stdout).contains("---"));
    assert_eq!(fs::read(&fixture.config()).unwrap(), before_dry_run);
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
        .cli(&["install", "--codex-home", fixture.home_arg()])
        .status
        .success());
    let installed = fs::read(&fixture.config()).unwrap();

    let removed = fixture.cli(&["uninstall", "--codex-home", fixture.home_arg()]);
    assert!(removed.status.success());
    assert_eq!(fs::read(&fixture.config()).unwrap(), original.as_bytes());
    assert_eq!(backups(&fixture.config()).len(), 2);
    assert!(backups(&fixture.config())
        .iter()
        .any(|backup| fs::read(backup).unwrap() == installed));
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

    let installed = fixture.cli(&["install", "--codex-home", fixture.home_arg()]);
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
        .cli(&["install", "--codex-home", fixture.home_arg()])
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
        .cli(&["install", "--codex-home", fixture.home_arg()])
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
    let mut child = Command::new(env!("CARGO_BIN_EXE_prov-hook"))
        .arg("codex")
        .env("PROV_HOME", &fixture.store)
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
        .cli(&["install", "--codex-home", fixture.home_arg()])
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
    assert_eq!(snapshot(&fixture.root), before);
    fixture.drop_root();
}
