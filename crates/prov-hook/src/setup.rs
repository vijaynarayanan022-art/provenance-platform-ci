use similar::TextDiff;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

const OPEN_MARKER: &str = "# >>> prov-hook >>>";
const CLOSE_MARKER: &str = "# <<< prov-hook <<<";
const FEATURE_INSERTED: &str = "# prov-hook-feature: inserted";
const FEATURE_RESTORE_PREFIX: &str = "# prov-hook-feature: restore:";
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
    enable_hooks_feature: bool,
) -> Result<String, String> {
    let home = resolve_home(codex_home)?;
    let config = home.join("config.toml");
    let original = read_config(&config)?;
    refuse_conflicting_hooks(&original)?;
    let binary = binary_path(binary)?;
    let existing_metadata = feature_metadata(&original);
    let (feature_text, block_features, metadata) = if enable_hooks_feature {
        enable_hooks(&original, existing_metadata)?
    } else {
        (
            original.clone(),
            block_has_features(&original),
            existing_metadata,
        )
    };
    let block = managed_block(&binary, block_features, metadata.as_deref());
    let updated = install_block(&feature_text, &block)?;
    let warning = if binary.to_string_lossy().chars().any(char::is_whitespace) {
        "WARNING: executable path contains spaces; verify with doctor and a real Codex session.\n"
    } else {
        ""
    };
    let note = if enable_hooks_feature || feature_enabled(&updated) {
        String::new()
    } else {
        "NOTE: Codex hooks are not enabled; rerun with --enable-hooks-feature.\n".into()
    };
    if updated == original {
        return Ok(warning.to_owned()
            + &note
            + &if dry_run {
                "No changes.\n".into()
            } else {
                format!("Already installed in {}\n", config.display())
            });
    }
    if dry_run {
        return Ok(warning.to_owned() + &unified_diff(&config, &original, &updated) + &note);
    }
    backup_config(&config, &original)?;
    write_config(&config, &updated)?;
    Ok(
        warning.to_owned()
            + &note
            + &format!("Installed prov-hook hooks in {}\n", config.display()),
    )
}

fn refuse_conflicting_hooks(contents: &str) -> Result<(), String> {
    for (line_number, line) in contents.lines().enumerate() {
        let trimmed = line.trim_start();
        let event_assignment = ["PreToolUse", "PostToolUse", "Stop"].iter().any(|event| {
            trimmed
                .strip_prefix(event)
                .is_some_and(|rest| rest.trim_start().starts_with('='))
        });
        let inline_hooks = line.split_once('=').is_some_and(|(key, value)| {
            key.trim() == "hooks" && value.trim_start().starts_with('{')
        });
        if event_assignment || inline_hooks {
            return Err(format!(
                "refusing to install: conflicting hook syntax on config.toml line {}",
                line_number + 1
            ));
        }
    }
    Ok(())
}

pub fn uninstall(codex_home: Option<&Path>, dry_run: bool) -> Result<String, String> {
    let home = resolve_home(codex_home)?;
    let config = home.join("config.toml");
    let original = read_config(&config)?;
    let updated = uninstall_block(&original)?;
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

    let configured_binary = managed_command(&contents).map(PathBuf::from);
    let binary_runs = configured_binary.as_ref().is_some_and(|binary| {
        binary.is_file()
            && Command::new(binary)
                .arg("codex")
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .output()
                .is_ok_and(|output| {
                    output.status.success() && output.stdout.is_empty() && output.stderr.is_empty()
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
    check(
        &mut lines,
        &mut failed,
        feature_enabled(&contents),
        "hooks feature",
        "config.toml must have [features] with hooks = true; install with --enable-hooks-feature",
    );

    let expected_block = configured_binary
        .as_ref()
        .map(|binary| {
            managed_block(
                binary,
                block_has_features(&contents),
                feature_metadata(&contents).as_deref(),
            )
        })
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

    let marker_start = locate_block(&contents)
        .ok()
        .flatten()
        .map(|(start, _)| start);
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
    let default_store = default_store_home();
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
        match repo_diagnostics(repo, &store, &default_store) {
            Ok((repo_id, resolved_event_count, other_event_count, last_event)) => {
                lines.push(format!("PASS repo identity: {repo_id}"));
                lines.push(if resolved_event_count == 0 {
                    "WARN repo events: none found in resolved store".into()
                } else {
                    format!("PASS repo events: {resolved_event_count} event log(s) found")
                });
                if other_event_count > 0 {
                    lines.push(format!(
                        "WARN repo events: {other_event_count} event log(s) found in default store, not resolved store"
                    ));
                }
                lines.push(format!(
                    "PASS last event time: {}",
                    last_event
                        .map(|timestamp| format!("{timestamp} Unix ns"))
                        .unwrap_or_else(|| "none".into())
                ));
                match prov_capture::commit::pending_status_at(
                    repo,
                    &store.join("repos").join(&repo_id),
                ) {
                    Ok((pending, oldest)) => {
                        let now = SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .map(|duration| duration.as_nanos())
                            .unwrap_or_default();
                        let stale = oldest.is_some_and(|timestamp| {
                            now.saturating_sub(timestamp) > 300 * 1_000_000_000
                        });
                        let status = if pending > 0 && stale { "WARN" } else { "PASS" };
                        lines.push(format!(
                            "{status} pending commits: {pending} not yet derived"
                        ));
                    }
                    Err(_) => lines.push("WARN pending commits: unable to inspect".into()),
                }
            }
            Err(error) => check(&mut lines, &mut failed, false, "repo identity", &error),
        }
    }
    if std::env::var_os("PROV_HOME").is_some() && !same_directory(&store, &default_store) {
        lines.push(format!(
            "WARN PROV_HOME differs from default store: resolved {}, default {}",
            store.display(),
            default_store.display()
        ));
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

fn managed_block(binary: &Path, features: bool, metadata: Option<&str>) -> String {
    let path = binary.to_string_lossy();
    let command = if !path.chars().any(char::is_whitespace) && !path.contains('\'') {
        format!("'{path} codex'")
    } else {
        toml_basic_string(&format!("\"{path}\" codex"))
    };
    let mut block = String::from(OPEN_MARKER);
    if features {
        block.push_str("\n\n[features]\nhooks = true # prov-hook");
    }
    for event in ["PreToolUse", "PostToolUse", "Stop"] {
        block.push_str(&format!(
            "\n\n[[hooks.{event}]]\n[[hooks.{event}.hooks]]\ntype = \"command\"\ncommand = {command}"
        ));
    }
    if let Some(metadata) = metadata {
        block.push('\n');
        block.push_str(metadata);
    }
    block.push('\n');
    block.push_str(CLOSE_MARKER);
    block
}

fn block_has_features(contents: &str) -> bool {
    locate_block(contents)
        .ok()
        .flatten()
        .is_some_and(|(start, end)| {
            contents[start..end]
                .lines()
                .any(|line| line.trim() == "[features]")
        })
}

fn feature_metadata(contents: &str) -> Option<String> {
    let (start, end) = locate_block(contents).ok()??;
    contents[start..end]
        .lines()
        .find(|line| {
            line.trim() == FEATURE_INSERTED || line.trim().starts_with(FEATURE_RESTORE_PREFIX)
        })
        .map(|line| line.trim().to_owned())
}

fn feature_enabled(contents: &str) -> bool {
    let mut in_features = false;
    for line in contents.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_features = trimmed == "[features]";
        } else if in_features
            && trimmed.split_once('=').is_some_and(|(key, value)| {
                key.trim() == "hooks" && value.trim().starts_with("true")
            })
        {
            return true;
        }
    }
    false
}

fn enable_hooks(
    original: &str,
    old_metadata: Option<String>,
) -> Result<(String, bool, Option<String>), String> {
    if let Some(metadata) = old_metadata {
        return Ok((
            original.to_owned(),
            block_has_features(original),
            Some(metadata),
        ));
    }
    let mut features_header = None;
    for (start, line) in line_offsets(original) {
        if line.trim() == "[features]" {
            if features_header.is_some() {
                return Err("config.toml has multiple [features] tables".into());
            }
            features_header = Some((start, line));
        }
    }
    let Some((header_start, header_line)) = features_header else {
        return Ok((original.to_owned(), true, None));
    };
    let header_end = header_start + header_line.len();
    let section_end = line_offsets(&original[header_end..])
        .into_iter()
        .find_map(|(offset, line)| {
            line.trim_start()
                .starts_with('[')
                .then_some(header_end + offset)
        })
        .unwrap_or(original.len());
    let section = &original[header_end..section_end];
    let hooks_line = line_offsets(section).into_iter().find(|(_, line)| {
        line.trim_start().starts_with("hooks")
            && line.trim_start()[5..].trim_start().starts_with('=')
    });
    let (updated, metadata) = if let Some((offset, line)) = hooks_line {
        let current = line.trim();
        let Some((_, value)) = current.split_once('=') else {
            return Err("malformed hooks feature setting".into());
        };
        if value.trim().starts_with("true") {
            return Ok((original.to_owned(), false, None));
        }
        if !value.trim().starts_with("false") {
            return Err("[features].hooks must be true or false".into());
        }
        let newline = line_ending(line);
        let replacement = format!("hooks = true # prov-hook (was false){newline}");
        let absolute = header_end + offset;
        let updated = format!(
            "{}{}{}",
            &original[..absolute],
            replacement,
            &original[absolute + line.len()..]
        );
        let metadata = format!("{FEATURE_RESTORE_PREFIX}{}", hex_encode(line.as_bytes()));
        (updated, Some(metadata))
    } else {
        let newline = line_ending(header_line);
        let insert = format!("hooks = true # prov-hook{newline}");
        let updated = format!(
            "{}{}{}",
            &original[..header_end],
            insert,
            &original[header_end..]
        );
        (updated, Some(FEATURE_INSERTED.to_owned()))
    };
    Ok((updated, false, metadata))
}

fn uninstall_block(original: &str) -> Result<String, String> {
    let metadata = feature_metadata(original);
    let mut updated = remove_block(original)?;
    match metadata.as_deref() {
        Some(FEATURE_INSERTED) => {
            updated = remove_feature_line(&updated, "hooks = true # prov-hook");
        }
        Some(metadata) if metadata.starts_with(FEATURE_RESTORE_PREFIX) => {
            if let Some(original_line) = hex_decode(&metadata[FEATURE_RESTORE_PREFIX.len()..]) {
                updated = replace_feature_line(
                    &updated,
                    "hooks = true # prov-hook (was false)",
                    &original_line,
                );
            }
        }
        _ => {}
    }
    Ok(updated)
}

fn line_offsets(text: &str) -> Vec<(usize, &str)> {
    let mut offset = 0;
    text.split_inclusive('\n')
        .map(|line| {
            let current = (offset, line);
            offset += line.len();
            current
        })
        .collect()
}

fn line_ending(line: &str) -> &'static str {
    if line.ends_with("\r\n") {
        "\r\n"
    } else {
        "\n"
    }
}

fn remove_feature_line(contents: &str, target: &str) -> String {
    replace_feature_line(contents, target, "")
}

fn replace_feature_line(contents: &str, target: &str, replacement: &str) -> String {
    let mut in_features = false;
    let mut result = String::with_capacity(contents.len());
    for line in line_offsets(contents) {
        let text = line.1;
        let trimmed = text.trim_end_matches(['\r', '\n']).trim();
        if trimmed.starts_with('[') {
            in_features = trimmed == "[features]";
        }
        if in_features && trimmed == target {
            if !replacement.is_empty() {
                let newline = line_ending(text);
                result.push_str(replacement.trim_end_matches(['\r', '\n']));
                result.push_str(newline);
            }
        } else {
            result.push_str(text);
        }
    }
    result
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn hex_decode(value: &str) -> Option<String> {
    if !value.len().is_multiple_of(2) {
        return None;
    }
    let bytes = (0..value.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&value[index..index + 2], 16).ok())
        .collect::<Option<Vec<_>>>()?;
    String::from_utf8(bytes).ok()
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
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    for attempt in 0..1000 {
        let temporary = parent.join(format!(
            ".config.toml.prov-{}-{nonce}-{attempt}",
            std::process::id()
        ));
        let mut file = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(format!("could not create config temp file: {error}")),
        };
        if let Err(error) = file
            .write_all(contents.as_bytes())
            .and_then(|_| file.flush())
        {
            let _ = fs::remove_file(&temporary);
            return Err(format!("could not write config temp file: {error}"));
        }
        drop(file);
        if let Err(error) = fs::rename(&temporary, path) {
            let _ = fs::remove_file(&temporary);
            return Err(format!("could not replace {}: {error}", path.display()));
        }
        return Ok(());
    }
    Err("could not allocate config temp file".into())
}

fn unified_diff(path: &Path, before: &str, after: &str) -> String {
    if before == after {
        return "No changes.\n".into();
    }
    TextDiff::from_lines(before, after)
        .unified_diff()
        .context_radius(3)
        .header(
            &format!("a/{}", path.display()),
            &format!("b/{}", path.display()),
        )
        .to_string()
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

fn trusted_state_entry(
    contents: &str,
    config_path: &Path,
    snake_event: &str,
    group: usize,
) -> bool {
    let key = format!("{}:{snake_event}:{group}:0", config_path.display());
    let literal_header = format!("[hooks.state.'{key}']");
    let basic_header = format!("[hooks.state.{}]", toml_basic_string(&key));
    let lines: Vec<_> = contents.lines().collect();
    let Some(start) = lines.iter().position(
        |line| matches!(line.trim(), value if value == literal_header || value == basic_header),
    ) else {
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
    default_store_home()
}

fn default_store_home() -> PathBuf {
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

fn repo_diagnostics(
    repo: &Path,
    resolved_store: &Path,
    default_store: &Path,
) -> Result<(String, usize, usize, Option<u128>), String> {
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
    let repo_id = cached_repo_id(resolved_store, &root_check)
        .or_else(|| cached_repo_id(default_store, &root_check))
        .ok_or("no cached repo identity in resolved or default store; run a hook once")?;
    let (resolved_event_count, resolved_last) = event_stats(resolved_store, &repo_id);
    let (other_event_count, other_last) = if same_directory(resolved_store, default_store) {
        (0, None)
    } else {
        event_stats(default_store, &repo_id)
    };
    let last_event = match (resolved_last, other_last) {
        (Some(left), Some(right)) => Some(left.max(right)),
        (left, right) => left.or(right),
    };
    Ok((repo_id, resolved_event_count, other_event_count, last_event))
}

fn cached_repo_id(store: &Path, root_check: &str) -> Option<String> {
    let entries = fs::read_dir(store.join("repo-cache")).ok()?;
    for entry in entries.flatten() {
        let Ok(contents) = fs::read_to_string(entry.path()) else {
            continue;
        };
        if json_string_field(&contents, "root_check").as_deref() == Some(root_check) {
            return json_string_field(&contents, "repo_id");
        }
    }
    None
}

fn event_stats(store: &Path, repo_id: &str) -> (usize, Option<u128>) {
    let event_dir = store.join("repos").join(repo_id).join("events");
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
    (event_files, last_event)
}

fn same_directory(left: &Path, right: &Path) -> bool {
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => left == right,
    }
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
        let block = managed_block(Path::new("C:\\Program Files\\prov-hook.exe"), false, None);
        let installed = install_block(original, &block).unwrap();
        assert!(installed.starts_with(original));
        assert_eq!(remove_block(&installed).unwrap(), original);
    }
}
