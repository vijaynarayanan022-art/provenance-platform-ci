use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

const OPEN_MARKER: &str = "# >>> prov-hook >>>";
const CLOSE_MARKER: &str = "# <<< prov-hook <<<";
const HOOK_EVENTS: [(&str, &str); 3] = [
    ("PreToolUse", "pre_tool_use"),
    ("PostToolUse", "post_tool_use"),
    ("Stop", "stop"),
];

pub struct DoctorReport {
    pub lines: Vec<String>,
    pub failed: bool,
}

pub fn install(
    codex_home: Option<&Path>,
    binary: Option<&Path>,
    dry_run: bool,
) -> Result<String, String> {
    let home = resolve_home(codex_home)?;
    let config = home.join("config.toml");
    let original = read_config(&config)?;
    let binary = binary_path(binary)?;
    let block = managed_block(&binary);
    let updated = install_block(&original, &block)?;
    if updated == original {
        return Ok(if dry_run {
            "No changes.\n".into()
        } else {
            format!("Already installed in {}\n", config.display())
        });
    }
    if dry_run {
        return Ok(unified_diff(&config, &original, &updated));
    }
    backup_config(&config, &original)?;
    write_config(&config, &updated)?;
    Ok(format!(
        "Installed prov-hook hooks in {}\n",
        config.display()
    ))
}

pub fn uninstall(codex_home: Option<&Path>, dry_run: bool) -> Result<String, String> {
    let home = resolve_home(codex_home)?;
    let config = home.join("config.toml");
    let original = read_config(&config)?;
    let updated = remove_block(&original)?;
    if updated == original {
        return Ok(if dry_run {
            "No changes.\n".into()
        } else {
            format!("No prov-hook block in {}\n", config.display())
        });
    }
    if dry_run {
        return Ok(unified_diff(&config, &original, &updated));
    }
    backup_config(&config, &original)?;
    write_config(&config, &updated)?;
    Ok(format!(
        "Removed prov-hook hooks from {}\n",
        config.display()
    ))
}

pub fn doctor(codex_home: Option<&Path>, repo: Option<&Path>) -> Result<DoctorReport, String> {
    let home = resolve_home(codex_home)?;
    let config = home.join("config.toml");
    let config_exists = config.is_file();
    let contents = read_config(&config)?;
    let current_binary = std::env::current_exe().map_err(|error| error.to_string())?;
    let mut lines = Vec::new();
    let mut failed = false;

    let configured_binary = managed_command(&contents).map(|command| PathBuf::from(command));
    let binary_runs = configured_binary.as_ref().is_some_and(|binary| {
        binary.is_file()
            && Command::new(binary)
                .arg("codex")
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .output()
                .is_ok_and(|output| {
                    output.status.success()
                        && output.stdout.is_empty()
                        && output.stderr.is_empty()
                })
    });
    if let Some(binary) = configured_binary.as_ref() {
        if binary_runs {
            if same_file(binary, &current_binary) {
                lines.push("PASS binary: configured executable runs silently".into());
            } else {
                lines.push(format!(
                    "WARN binary: configured executable differs from running binary ({})",
                    binary.display()
                ));
            }
        } else {
            check(
                &mut lines,
                &mut failed,
                false,
                "binary",
                &format!("{} is missing or did not run silently", binary.display()),
            );
        }
    } else {
        check(
            &mut lines,
            &mut failed,
            false,
            "binary",
            "managed block has no command executable",
        );
    }
    check(
        &mut lines,
        &mut failed,
        config_exists,
        "config.toml",
        &config.display().to_string(),
    );

    let expected_block = configured_binary
        .as_ref()
        .map(|binary| managed_block(binary))
        .unwrap_or_default();
    let block_matches = locate_block(&contents)
        .ok()
        .flatten()
        .is_some_and(|(start, end)| contents[start..end] == expected_block);
    check(
        &mut lines,
        &mut failed,
        block_matches,
        "managed block",
        "present and matches this executable",
    );

    for event in ["PreToolUse", "PostToolUse", "Stop"] {
        let entry = format!("[[hooks.{event}]]");
        check(
            &mut lines,
            &mut failed,
            contents.contains(&entry),
            "hook entry",
            event,
        );
    }

    let marker_start = locate_block(&contents).ok().flatten().map(|(start, _)| start);
    let trusted = marker_start.is_some_and(|start| {
        HOOK_EVENTS.iter().all(|(event, snake)| {
            let group_index = hook_group_index(&contents[..start], event);
            trusted_state_entry(&contents, &config, snake, group_index)
        })
    });
    lines.push(if trusted {
        "PASS hook state/trust: all three entries are trusted and not disabled".into()
    } else {
        "WARN hook state/trust: open Codex and approve hooks".into()
    });

    let store = store_home();
    let (store_ok, store_detail) = store_writable(&store);
    check(
        &mut lines,
        &mut failed,
        store_ok,
        "store directory writable",
        &store_detail,
    );

    let git_available = git(&std::env::current_dir().unwrap_or_default(), &["--version"])
        .is_ok_and(|output| output.status.success());
    check(&mut lines, &mut failed, git_available, "git", "available");

    if let Some(repo) = repo {
        match repo_diagnostics(repo, &store) {
            Ok((repo_id, event_count, last_event)) => {
                lines.push(format!("PASS repo identity: {repo_id}"));
                lines.push(if event_count == 0 {
                    "WARN repo events: none found".into()
                } else {
                    format!("PASS repo events: {event_count} event log(s) found")
                });
                lines.push(format!(
                    "PASS last event time: {}",
                    last_event
                        .map(|timestamp| format!("{timestamp} Unix ns"))
                        .unwrap_or_else(|| "none".into())
                ));
            }
            Err(error) => check(&mut lines, &mut failed, false, "repo identity", &error),
        }
    }

    Ok(DoctorReport { lines, failed })
}

fn resolve_home(home: Option<&Path>) -> Result<PathBuf, String> {
    let selected = if let Some(home) = home {
        home.to_path_buf()
    } else if let Some(home) = std::env::var_os("CODEX_HOME").filter(|value| !value.is_empty()) {
        PathBuf::from(home)
    } else {
        let profile = std::env::var_os("USERPROFILE")
            .or_else(|| std::env::var_os("HOME"))
            .ok_or("could not determine home directory")?;
        PathBuf::from(profile).join(".codex")
    };
    absolute_path(&selected)
}

fn binary_path(binary: Option<&Path>) -> Result<PathBuf, String> {
    let path = if let Some(path) = binary {
        path.to_path_buf()
    } else {
        std::env::current_exe().map_err(|error| error.to_string())?
    };
    absolute_path(&path)
}

fn absolute_path(path: &Path) -> Result<PathBuf, String> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()
            .map_err(|error| error.to_string())?
            .join(path))
    }
}

fn managed_block(binary: &Path) -> String {
    let command = format!("\"{}\" codex", binary.to_string_lossy());
    let command = toml_basic_string(&command);
    let mut block = String::from(OPEN_MARKER);
    for event in ["PreToolUse", "PostToolUse", "Stop"] {
        block.push_str(&format!(
            "\n\n[[hooks.{event}]]\n[[hooks.{event}.hooks]]\ntype = \"command\"\ncommand = {command}"
        ));
    }
    block.push('\n');
    block.push_str(CLOSE_MARKER);
    block
}

fn managed_command(contents: &str) -> Option<String> {
    let (start, end) = locate_block(contents).ok()??;
    let block = &contents[start..end];
    let command = block
        .lines()
        .find_map(|line| line.trim().strip_prefix("command ="))?
        .trim();
    let value = parse_toml_string(command)?;
    if let Some(quoted) = value.strip_prefix('"') {
        let end = quoted.find('"')?;
        Some(quoted[..end].to_owned())
    } else {
        value.split_whitespace().next().map(str::to_owned)
    }
}

fn parse_toml_string(value: &str) -> Option<String> {
    if let Some(literal) = value.strip_prefix('\'') {
        return Some(literal.strip_suffix('\'')?.to_owned());
    }
    let basic = value.strip_prefix('"')?.strip_suffix('"')?;
    let mut output = String::new();
    let mut chars = basic.chars();
    while let Some(character) = chars.next() {
        if character != '\\' {
            output.push(character);
            continue;
        }
        output.push(match chars.next()? {
            '\\' => '\\',
            '"' => '"',
            'n' => '\n',
            'r' => '\r',
            't' => '\t',
            _ => return None,
        });
    }
    Some(output)
}

fn same_file(left: &Path, right: &Path) -> bool {
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => left == right,
    }
}

fn toml_basic_string(value: &str) -> String {
    let mut quoted = String::from("\"");
    for character in value.chars() {
        match character {
            '\\' => quoted.push_str("\\\\"),
            '"' => quoted.push_str("\\\""),
            '\n' => quoted.push_str("\\n"),
            '\r' => quoted.push_str("\\r"),
            '\t' => quoted.push_str("\\t"),
            character if character.is_control() => {
                quoted.push_str(&format!("\\u{:04X}", character as u32));
            }
            character => quoted.push(character),
        }
    }
    quoted.push('"');
    quoted
}

fn install_block(original: &str, block: &str) -> Result<String, String> {
    if let Some((start, end)) = locate_block(original)? {
        return Ok(format!(
            "{}{}{}",
            &original[..start],
            block,
            &original[end..]
        ));
    }
    let mut updated = original.to_owned();
    if !updated.is_empty() && !updated.ends_with('\n') {
        updated.push('\n');
    }
    updated.push_str(block);
    updated.push('\n');
    Ok(updated)
}

fn remove_block(original: &str) -> Result<String, String> {
    let Some((start, end)) = locate_block(original)? else {
        return Ok(original.to_owned());
    };
    let mut after = end;
    if original.get(after..) == Some("\n") {
        after += 1;
    }
    Ok(format!("{}{}", &original[..start], &original[after..]))
}

fn locate_block(text: &str) -> Result<Option<(usize, usize)>, String> {
    let opens: Vec<_> = text
        .match_indices(OPEN_MARKER)
        .map(|(index, _)| index)
        .collect();
    let closes: Vec<_> = text
        .match_indices(CLOSE_MARKER)
        .map(|(index, _)| index)
        .collect();
    if opens.is_empty() && closes.is_empty() {
        return Ok(None);
    }
    if opens.len() != 1 || closes.len() != 1 || opens[0] > closes[0] {
        return Err("config.toml has malformed prov-hook markers".into());
    }
    Ok(Some((opens[0], closes[0] + CLOSE_MARKER.len())))
}

fn read_config(path: &Path) -> Result<String, String> {
    match fs::read_to_string(path) {
        Ok(contents) => Ok(contents),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(format!("could not read {}: {error}", path.display())),
    }
}

fn backup_config(path: &Path, contents: &str) -> Result<PathBuf, String> {
    let parent = path.parent().ok_or("config.toml has no parent directory")?;
    fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    for offset in 0..10_000 {
        let backup = PathBuf::from(format!(
            "{}.prov-backup-{}",
            path.display(),
            backup_timestamp(offset)
        ));
        let mut file = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&backup)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(format!("could not create backup: {error}")),
        };
        file.write_all(contents.as_bytes())
            .map_err(|error| format!("could not write backup {}: {error}", backup.display()))?;
        return Ok(backup);
    }
    Err("could not choose a unique backup timestamp".into())
}

fn backup_timestamp(offset_seconds: u64) -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .saturating_add(offset_seconds);
    let days = (seconds / 86_400) as i64;
    let day_seconds = seconds % 86_400;
    let (year, month, day) = civil_from_days(days);
    let hour = day_seconds / 3_600;
    let minute = day_seconds / 60 % 60;
    let second = day_seconds % 60;
    format!("{year:04}{month:02}{day:02}{hour:02}{minute:02}{second:02}")
}

fn civil_from_days(days_since_epoch: i64) -> (i64, i64, i64) {
    let shifted = days_since_epoch + 719_468;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month, day)
}

fn write_config(path: &Path, contents: &str) -> Result<(), String> {
    let parent = path.parent().ok_or("config.toml has no parent directory")?;
    fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    fs::write(path, contents)
        .map_err(|error| format!("could not write {}: {error}", path.display()))
}

fn unified_diff(path: &Path, before: &str, after: &str) -> String {
    if before == after {
        return "No changes.\n".into();
    }
    let before_lines: Vec<_> = before.split_inclusive('\n').collect();
    let after_lines: Vec<_> = after.split_inclusive('\n').collect();
    let mut diff = format!(
        "--- {}\n+++ {}\n@@ -1,{} +1,{} @@\n",
        path.display(),
        path.display(),
        before_lines.len(),
        after_lines.len()
    );
    for line in before_lines {
        diff.push('-');
        diff.push_str(line);
        if !line.ends_with('\n') {
            diff.push('\n');
        }
    }
    for line in after_lines {
        diff.push('+');
        diff.push_str(line);
        if !line.ends_with('\n') {
            diff.push('\n');
        }
    }
    diff
}

fn check(lines: &mut Vec<String>, failed: &mut bool, passed: bool, name: &str, detail: &str) {
    let status = if passed { "PASS" } else { "FAIL" };
    *failed |= !passed;
    lines.push(format!("{status} {name}: {detail}"));
}

fn hook_group_index(before_block: &str, event: &str) -> usize {
    let table = format!("[[hooks.{event}]]");
    before_block
        .lines()
        .filter(|line| line.trim() == table)
        .count()
}

fn trusted_state_entry(contents: &str, config_path: &Path, snake_event: &str, group: usize) -> bool {
    let key = format!("{}:{snake_event}:{group}:0", config_path.display());
    let literal_header = format!("[hooks.state.'{key}']");
    let basic_header = format!("[hooks.state.{}]", toml_basic_string(&key));
    let lines: Vec<_> = contents.lines().collect();
    let Some(start) = lines
        .iter()
        .position(|line| matches!(line.trim(), value if value == literal_header || value == basic_header))
    else {
        return false;
    };
    let body = lines[start + 1..]
        .iter()
        .take_while(|line| !line.trim_start().starts_with('['));
    let mut has_hash = false;
    let mut disabled = false;
    for line in body {
        let trimmed = line.trim();
        if let Some(value) = trimmed
            .strip_prefix("trusted_hash")
            .and_then(|value| value.trim().strip_prefix('='))
        {
            let value = value.trim().trim_matches('"');
            has_hash = !value.is_empty();
        } else if trimmed == "enabled = false" {
            disabled = true;
        }
    }
    has_hash && !disabled
}

fn store_home() -> PathBuf {
    if let Some(path) = std::env::var_os("PROV_HOME") {
        return PathBuf::from(path);
    }
    #[cfg(windows)]
    {
        return std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."))
            .join("prov");
    }
    #[cfg(target_os = "macos")]
    {
        return std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."))
            .join("Library/Application Support/prov");
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        return std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share"))
            })
            .unwrap_or_else(|| PathBuf::from("."))
            .join("prov");
    }
    #[allow(unreachable_code)]
    PathBuf::from("prov")
}

fn store_writable(path: &Path) -> (bool, String) {
    let mut candidate = path;
    while !candidate.exists() {
        let Some(parent) = candidate.parent() else {
            return (false, format!("no existing parent for {}", path.display()));
        };
        candidate = parent;
    }
    match fs::metadata(candidate) {
        Ok(metadata) if metadata.is_dir() && !metadata.permissions().readonly() => (
            true,
            format!(
                "{} (read-only metadata probe; no write performed)",
                path.display()
            ),
        ),
        Ok(_) => (
            false,
            format!("{} has no writable directory ancestor", path.display()),
        ),
        Err(error) => (false, error.to_string()),
    }
}

fn git(repo: &Path, args: &[&str]) -> Result<std::process::Output, String> {
    Command::new("git")
        .args(args)
        .current_dir(repo)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .output()
        .map_err(|error| error.to_string())
}

fn repo_diagnostics(repo: &Path, store: &Path) -> Result<(String, usize, Option<u128>), String> {
    let root = git(repo, &["rev-parse", "--show-toplevel"])?;
    if !root.status.success() {
        return Err("not a Git repository".into());
    }
    let root = PathBuf::from(String::from_utf8_lossy(&root.stdout).trim());
    let roots = git(
        &root,
        &["rev-list", "--first-parent", "--max-parents=0", "HEAD"],
    )?;
    if !roots.status.success() {
        return Err("repository has no resolvable root commit".into());
    }
    let mut root_ids: Vec<_> = String::from_utf8_lossy(&roots.stdout)
        .lines()
        .map(str::to_owned)
        .collect();
    root_ids.sort();
    if root_ids.is_empty() {
        return Err("repository has no root commits".into());
    }
    let root_check = root_ids.join("\n");
    let cache_dir = store.join("repo-cache");
    let entries =
        fs::read_dir(&cache_dir).map_err(|_| "no cached repo identity; run a hook once")?;
    let mut repo_id = None;
    for entry in entries.flatten() {
        let Ok(contents) = fs::read_to_string(entry.path()) else {
            continue;
        };
        if json_string_field(&contents, "root_check").as_deref() == Some(root_check.as_str()) {
            repo_id = json_string_field(&contents, "repo_id");
            break;
        }
    }
    let repo_id = repo_id.ok_or("no cached repo identity for this repository; run a hook once")?;
    let event_dir = store.join("repos").join(&repo_id).join("events");
    let mut event_files = 0;
    let mut last_event = None;
    if let Ok(entries) = fs::read_dir(event_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path
                .extension()
                .is_none_or(|extension| extension != "jsonl")
            {
                continue;
            }
            event_files += 1;
            let Ok(bytes) = fs::read(path) else {
                continue;
            };
            for line in bytes.split(|byte| *byte == b'\n') {
                if let Some(timestamp) = json_u128_field(line, "timestamp") {
                    last_event =
                        Some(last_event.map_or(timestamp, |last: u128| last.max(timestamp)));
                }
            }
        }
    }
    Ok((repo_id, event_files, last_event))
}

fn json_string_field(text: &str, field: &str) -> Option<String> {
    let key = format!("\"{field}\"");
    let mut rest = text.get(text.find(&key)? + key.len()..)?.trim_start();
    rest = rest.strip_prefix(':')?.trim_start();
    let mut chars = rest.strip_prefix('"')?.chars();
    let mut value = String::new();
    while let Some(character) = chars.next() {
        match character {
            '"' => return Some(value),
            '\\' => match chars.next()? {
                'n' => value.push('\n'),
                'r' => value.push('\r'),
                't' => value.push('\t'),
                '"' => value.push('"'),
                '\\' => value.push('\\'),
                '/' => value.push('/'),
                _ => return None,
            },
            character => value.push(character),
        }
    }
    None
}

fn json_u128_field(bytes: &[u8], field: &str) -> Option<u128> {
    let text = std::str::from_utf8(bytes).ok()?;
    let key = format!("\"{field}\"");
    let rest = text.get(text.find(&key)? + key.len()..)?.trim_start();
    let value = rest.strip_prefix(':')?.trim_start();
    let digits: String = value.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::{install_block, managed_block, remove_block};
    use std::path::Path;

    #[test]
    fn block_insertion_and_removal_preserve_existing_text() {
        let original = "model = \"kept\"\n[other]\nkey = true\n";
        let block = managed_block(Path::new("C:\\Program Files\\prov-hook.exe"));
        let installed = install_block(original, &block).unwrap();
        assert!(installed.starts_with(original));
        assert_eq!(remove_block(&installed).unwrap(), original);
    }
}
