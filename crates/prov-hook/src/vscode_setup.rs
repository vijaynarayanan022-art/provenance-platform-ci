use serde_json::{json, Map, Value};
use std::fs;
use std::path::{Path, PathBuf};

const EVENTS: [&str; 3] = ["PreToolUse", "PostToolUse", "Stop"];
const LEGACY_EVENTS: [&str; 3] = ["preToolUse", "postToolUse", "agentStop"];

pub fn install(
    home: Option<&Path>,
    binary: Option<&Path>,
    dry_run: bool,
) -> Result<String, String> {
    let hooks_file = hooks_file(home)?;
    let binary = absolute_path(binary)?;
    let mut document = read_document(&hooks_file)?;
    let original = document.clone();
    let root = document
        .as_object_mut()
        .ok_or("hooks file must contain a JSON object")?;
    root.entry("version").or_insert(json!(1));
    let hooks = root
        .entry("hooks")
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .ok_or("hooks must be a JSON object")?;
    for event in LEGACY_EVENTS {
        if let Some(entries) = hooks.get_mut(event).and_then(Value::as_array_mut) {
            entries.retain(|entry| !is_ours(entry));
        }
    }
    for event in EVENTS {
        let entries = hooks
            .entry(event)
            .or_insert_with(|| Value::Array(Vec::new()))
            .as_array_mut()
            .ok_or_else(|| format!("hooks.{event} must be a JSON array"))?;
        entries.retain(|entry| !is_ours(entry));
        entries.push(command_entry(&binary));
    }
    if document != original {
        if dry_run {
            return Ok(format!(
                "Would install prov-hook VS Code hooks in {}\n",
                hooks_file.display()
            ));
        }
        write_document(&hooks_file, &document)?;
        Ok(format!(
            "Installed prov-hook VS Code hooks in {}\n",
            hooks_file.display()
        ))
    } else {
        Ok(format!("Already installed in {}\n", hooks_file.display()))
    }
}

pub fn uninstall(home: Option<&Path>, dry_run: bool) -> Result<String, String> {
    let hooks_file = hooks_file(home)?;
    let mut document = match fs::read(&hooks_file) {
        Ok(bytes) => serde_json::from_slice::<Value>(&bytes)
            .map_err(|error| format!("invalid {}: {error}", hooks_file.display()))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(format!("No prov-hook hooks in {}\n", hooks_file.display()))
        }
        Err(error) => return Err(format!("could not read {}: {error}", hooks_file.display())),
    };
    let Some(hooks) = document.get_mut("hooks").and_then(Value::as_object_mut) else {
        return Ok(format!("No prov-hook hooks in {}\n", hooks_file.display()));
    };
    let mut changed = false;
    for event in EVENTS.into_iter().chain(LEGACY_EVENTS) {
        if let Some(entries) = hooks.get_mut(event).and_then(Value::as_array_mut) {
            let before = entries.len();
            entries.retain(|entry| !is_ours(entry));
            changed |= entries.len() != before;
        }
    }
    if !changed {
        return Ok(format!("No prov-hook hooks in {}\n", hooks_file.display()));
    }
    if dry_run {
        return Ok(format!(
            "Would remove prov-hook VS Code hooks from {}\n",
            hooks_file.display()
        ));
    }
    if only_empty_prov_document(&document) {
        fs::remove_file(&hooks_file)
            .map_err(|error| format!("could not remove {}: {error}", hooks_file.display()))?;
    } else {
        write_document(&hooks_file, &document)?;
    }
    Ok(format!(
        "Removed prov-hook VS Code hooks from {}\n",
        hooks_file.display()
    ))
}

pub fn doctor(home: Option<&Path>) -> Result<DoctorReport, String> {
    let hooks_file = hooks_file(home)?;
    let exists = hooks_file.is_file();
    let bytes = fs::read(&hooks_file).ok();
    let parsed = bytes
        .as_deref()
        .and_then(|bytes| serde_json::from_slice::<Value>(bytes).ok());
    let current_binary = std::env::current_exe().map_err(|error| error.to_string())?;
    let command_ok = parsed.as_ref().is_some_and(|document| {
        let Some(hooks) = document.get("hooks").and_then(Value::as_object) else {
            return false;
        };
        EVENTS.iter().all(|event| {
            hooks
                .get(*event)
                .and_then(Value::as_array)
                .is_some_and(|entries| {
                    let matching: Vec<_> = entries.iter().filter(|entry| is_ours(entry)).collect();
                    matching.len() == 1
                        && command_paths(matching[0]).is_some_and(|(powershell, bash)| {
                            powershell.is_file()
                                && bash.is_file()
                                && same_file(powershell, &current_binary)
                                && same_file(bash, &current_binary)
                        })
                })
        }) && !LEGACY_EVENTS.iter().any(|event| {
            hooks
                .get(*event)
                .and_then(Value::as_array)
                .is_some_and(|entries| entries.iter().any(is_ours))
        })
    });
    let json_ok = parsed.is_some();
    let mut failed = false;
    let legacy_only = parsed.as_ref().is_some_and(|document| {
        let Some(hooks) = document.get("hooks").and_then(Value::as_object) else {
            return false;
        };
        LEGACY_EVENTS.iter().any(|event| {
            hooks
                .get(*event)
                .and_then(Value::as_array)
                .is_some_and(|entries| entries.iter().any(is_ours))
        }) && !EVENTS.iter().any(|event| {
            hooks
                .get(*event)
                .and_then(Value::as_array)
                .is_some_and(|entries| entries.iter().any(is_ours))
        })
    });
    let lines = [
        check(
            &mut failed,
            exists,
            "hooks file present",
            &hooks_file.display().to_string(),
        ),
        check(
            &mut failed,
            json_ok,
            "valid JSON",
            &hooks_file.display().to_string(),
        ),
        check(
            &mut failed,
            command_ok,
            "command path",
            "configured command exists and points to this executable",
        ),
        check(
            &mut failed,
            !legacy_only,
            "hook event format",
            if legacy_only {
                "only camelCase events are installed; VS Code Copilot CLI selects its VS Code payload format with PascalCase PreToolUse, PostToolUse, and Stop events"
            } else {
                "PascalCase VS Code event names are configured"
            },
        ),
    ]
    .into_iter()
    .collect();
    Ok(DoctorReport { lines, failed })
}

pub struct DoctorReport {
    pub lines: Vec<String>,
    pub failed: bool,
}

fn command_entry(binary: &Path) -> Value {
    let path = binary.to_string_lossy();
    let powershell_path = path.replace('\'', "''");
    let bash_path = path.replace('\'', "'\\''");
    json!({
        "type": "command",
        "command": format!("'{bash_path}' vscode"),
        "powershell": format!("& '{powershell_path}' vscode"),
        "timeoutSec": 5,
    })
}

fn is_ours(entry: &Value) -> bool {
    entry.get("type").and_then(Value::as_str) == Some("command")
        && command_paths(entry)
            .is_some_and(|(powershell, command)| is_prov_hook(powershell) && is_prov_hook(command))
}

fn command_paths(entry: &Value) -> Option<(&Path, &Path)> {
    let powershell = entry.get("powershell")?.as_str()?;
    let command = entry
        .get("command")
        .or_else(|| entry.get("bash"))?
        .as_str()?;
    let powershell = powershell.strip_prefix("& '")?.strip_suffix("' vscode")?;
    let command = command
        .strip_prefix('\'')
        .or_else(|| command.strip_prefix("prov-hook.exe "))?;
    let command = command
        .strip_suffix("' vscode")
        .or_else(|| command.strip_suffix(" vscode"))?;
    Some((Path::new(powershell), Path::new(command)))
}

fn is_prov_hook(path: &Path) -> bool {
    path.file_stem()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case("prov-hook"))
}

fn only_empty_prov_document(document: &Value) -> bool {
    let Some(root) = document.as_object() else {
        return false;
    };
    if root.keys().any(|key| key != "version" && key != "hooks") {
        return false;
    }
    let Some(hooks) = root.get("hooks").and_then(Value::as_object) else {
        return true;
    };
    hooks.iter().all(|(event, entries)| {
        (EVENTS.contains(&event.as_str()) || LEGACY_EVENTS.contains(&event.as_str()))
            && entries.as_array().is_some_and(Vec::is_empty)
    })
}

fn read_document(path: &Path) -> Result<Value, String> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|error| format!("invalid {}: {error}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(json!({"version": 1, "hooks": {}}))
        }
        Err(error) => Err(format!("could not read {}: {error}", path.display())),
    }
}

fn write_document(path: &Path, document: &Value) -> Result<(), String> {
    let parent = path.parent().ok_or("hooks file has no parent directory")?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("could not create hooks directory: {error}"))?;
    let bytes = serde_json::to_vec_pretty(document).map_err(|error| error.to_string())?;
    fs::write(path, bytes).map_err(|error| format!("could not write {}: {error}", path.display()))
}

pub fn hooks_file_path(home: Option<&Path>) -> Result<PathBuf, String> {
    let home = home
        .map(Path::to_path_buf)
        .or_else(|| {
            std::env::var_os("COPILOT_HOME")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
        })
        .or_else(|| {
            std::env::var_os("USERPROFILE")
                .or_else(|| std::env::var_os("HOME"))
                .map(PathBuf::from)
                .map(|path| path.join(".copilot"))
        })
        .ok_or("could not determine Copilot home directory")?;
    Ok(home.join("hooks").join("prov-hook.json"))
}

fn hooks_file(home: Option<&Path>) -> Result<PathBuf, String> {
    hooks_file_path(home)
}

fn absolute_path(path: Option<&Path>) -> Result<PathBuf, String> {
    let path = match path {
        Some(path) if path.is_absolute() => path.to_path_buf(),
        Some(path) => std::env::current_dir()
            .map_err(|error| error.to_string())?
            .join(path),
        None => std::env::current_exe().map_err(|error| error.to_string())?,
    };
    Ok(path)
}

fn same_file(left: &Path, right: &Path) -> bool {
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => left == right,
    }
}

fn check(failed: &mut bool, passed: bool, name: &str, detail: &str) -> String {
    *failed |= !passed;
    format!("{} {name}: {detail}", if passed { "PASS" } else { "FAIL" })
}
