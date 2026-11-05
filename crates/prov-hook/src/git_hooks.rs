use std::env;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const OPEN_MARKER: &[u8] = b"# >>> prov-hook >>>";
const CLOSE_MARKER: &[u8] = b"# <<< prov-hook <<<";
const CREATED_MARKER: &[u8] = b"# prov-hook-created";
const ADDED_NEWLINE_MARKER: &[u8] = b"# prov-hook-added-newline";
const HOOK_NAMES: [&str; 2] = ["post-commit", "post-rewrite"];

#[derive(Clone, Copy)]
enum HookAction {
    Installed,
    AlreadyInstalled,
    Unsupported,
    Removed,
    NotPresent,
}

impl HookAction {
    fn as_str(self) -> &'static str {
        match self {
            Self::Installed => "installed",
            Self::AlreadyInstalled => "already installed",
            Self::Unsupported => "unsupported existing hook",
            Self::Removed => "removed",
            Self::NotPresent => "not present",
        }
    }
}

struct HookBlock {
    start: usize,
    end: usize,
    created: bool,
    added_newline: bool,
}

pub fn install(repo: &Path) -> Result<(String, bool), String> {
    let (_, hooks_dir) = resolve_repo_and_hooks(repo)?;
    let binary = env::current_exe().map_err(|error| error.to_string())?;
    fs::create_dir_all(&hooks_dir).map_err(|error| error.to_string())?;
    let mut output = String::new();
    let mut unsupported = false;
    for name in HOOK_NAMES {
        let action = install_hook(&hooks_dir.join(name), name, &binary)?;
        unsupported |= matches!(action, HookAction::Unsupported);
        output.push_str(&format!("{name}: {}\n", action.as_str()));
    }
    Ok((output, unsupported))
}

pub fn uninstall(repo: &Path) -> Result<String, String> {
    let (_, hooks_dir) = resolve_repo_and_hooks(repo)?;
    let mut output = String::new();
    for name in HOOK_NAMES {
        let action = uninstall_hook(&hooks_dir.join(name))?;
        output.push_str(&format!("{name}: {}\n", action.as_str()));
    }
    Ok(output)
}

pub fn managed_hook_paths(repo: &Path) -> Result<Vec<PathBuf>, String> {
    let (_, hooks_dir) = resolve_repo_and_hooks(repo)?;
    Ok(HOOK_NAMES.iter().map(|name| hooks_dir.join(name)).collect())
}

pub fn doctor_status(repo: &Path) -> String {
    match resolve_repo_and_hooks(repo) {
        Ok((_, hooks_dir)) => {
            let installed: Vec<_> = HOOK_NAMES
                .iter()
                .filter(|name| {
                    fs::read(hooks_dir.join(name))
                        .ok()
                        .is_some_and(|bytes| find_block(&bytes).is_some())
                })
                .copied()
                .collect();
            if installed.len() == HOOK_NAMES.len() {
                "PASS git hooks: post-commit and post-rewrite installed".into()
            } else if installed.is_empty() {
                "WARN git hooks: post-commit and post-rewrite not installed".into()
            } else {
                let missing = HOOK_NAMES
                    .iter()
                    .filter(|name| !installed.contains(name))
                    .copied()
                    .collect::<Vec<_>>()
                    .join(" and ");
                format!("WARN git hooks: {missing} not installed")
            }
        }
        Err(_) => "WARN git hooks: repository hooks unavailable".into(),
    }
}

pub fn repo_root(repo: &Path) -> Result<PathBuf, String> {
    let output = git_output(repo, &["rev-parse", "--show-toplevel"])?;
    if !output.status.success() {
        return Err("not a Git worktree".into());
    }
    let root = strip_verbatim_prefix(PathBuf::from(
        String::from_utf8_lossy(&output.stdout).trim(),
    ));
    root.canonicalize()
        .map(strip_verbatim_prefix)
        .or(Ok(root))
        .map_err(|error: std::io::Error| error.to_string())
}

pub fn head_commit(repo: &Path) -> Result<String, String> {
    let output = git_output(repo, &["rev-parse", "--verify", "HEAD^{commit}"])?;
    if !output.status.success() {
        return Err("HEAD is not a commit".into());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn resolve_repo_and_hooks(repo: &Path) -> Result<(PathBuf, PathBuf), String> {
    let root = repo_root(repo)?;
    let configured = git_output(&root, &["config", "--path", "--get", "core.hooksPath"])?;
    if configured.status.success() {
        let value = String::from_utf8_lossy(&configured.stdout)
            .trim()
            .to_owned();
        if !value.is_empty() {
            let path = strip_verbatim_prefix(PathBuf::from(value));
            let hooks = if path.is_absolute() {
                path
            } else {
                root.join(path)
            };
            return Ok((root, hooks));
        }
    }

    let output = git_output(&root, &["rev-parse", "--git-path", "hooks"])?;
    if !output.status.success() {
        return Err("could not resolve Git hooks directory".into());
    }
    let path = strip_verbatim_prefix(PathBuf::from(
        String::from_utf8_lossy(&output.stdout).trim(),
    ));
    let hooks = if path.is_absolute() {
        path
    } else {
        root.join(path)
    };
    Ok((root, hooks))
}

fn install_hook(path: &Path, name: &str, binary: &Path) -> Result<HookAction, String> {
    let existing = match fs::read(path) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.to_string()),
    };
    if let Some(bytes) = existing {
        if has_marker(&bytes) {
            let Some(block) = find_block(&bytes) else {
                return Ok(HookAction::Unsupported);
            };
            let expected = render_block(name, binary, block.created, block.added_newline);
            if bytes[block.start..block.end] == expected {
                return Ok(HookAction::AlreadyInstalled);
            }
            let mut updated = Vec::with_capacity(bytes.len() + expected.len());
            updated.extend_from_slice(&bytes[..block.start]);
            updated.extend_from_slice(&expected);
            updated.extend_from_slice(&bytes[block.end..]);
            fs::write(path, updated).map_err(|error| error.to_string())?;
            return Ok(HookAction::Installed);
        }
        let Some((shebang_end, has_newline)) = shell_shebang_end(&bytes) else {
            return Ok(HookAction::Unsupported);
        };
        let block = render_block(name, binary, false, !has_newline);
        let mut updated = Vec::with_capacity(bytes.len() + block.len() + 1);
        updated.extend_from_slice(&bytes[..shebang_end]);
        if !has_newline {
            updated.push(b'\n');
        }
        updated.extend_from_slice(&block);
        updated.extend_from_slice(&bytes[shebang_end..]);
        fs::write(path, updated).map_err(|error| error.to_string())?;
        return Ok(HookAction::Installed);
    }

    let mut contents = b"#!/bin/sh\n".to_vec();
    contents.extend_from_slice(&render_block(name, binary, true, false));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| error.to_string())?;
    file.write_all(&contents)
        .map_err(|error| error.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o755))
            .map_err(|error| error.to_string())?;
    }
    Ok(HookAction::Installed)
}

fn uninstall_hook(path: &Path) -> Result<HookAction, String> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(HookAction::NotPresent)
        }
        Err(error) => return Err(error.to_string()),
    };
    if !has_marker(&bytes) {
        return Ok(HookAction::NotPresent);
    }
    let Some(block) = find_block(&bytes) else {
        return Ok(HookAction::NotPresent);
    };
    let mut start = block.start;
    if block.added_newline && start > 0 && bytes[start - 1] == b'\n' {
        start -= 1;
    }
    let mut updated = Vec::with_capacity(bytes.len() - (block.end - start));
    updated.extend_from_slice(&bytes[..start]);
    updated.extend_from_slice(&bytes[block.end..]);
    if block.created && updated == b"#!/bin/sh\n" {
        fs::remove_file(path).map_err(|error| error.to_string())?;
    } else {
        fs::write(path, updated).map_err(|error| error.to_string())?;
    }
    Ok(HookAction::Removed)
}

fn render_block(name: &str, binary: &Path, created: bool, added_newline: bool) -> Vec<u8> {
    let binary = shell_quote(binary);
    let mut block = String::new();
    block.push_str("# >>> prov-hook >>>\n");
    if created {
        block.push_str("# prov-hook-created\n");
    }
    if added_newline {
        block.push_str("# prov-hook-added-newline\n");
    }
    if name == "post-rewrite" {
        block.push_str("prov_hook_stdin=$(mktemp 2>/dev/null) || prov_hook_stdin=\n");
        block.push_str("if [ -n \"$prov_hook_stdin\" ]; then\n");
        block.push_str("  cat > \"$prov_hook_stdin\"\n");
        block.push_str(&format!(
            "  [ -x {binary} ] && {binary} git-event post-rewrite \"$@\" < \"$prov_hook_stdin\" >/dev/null 2>&1 &\n"
        ));
        block.push_str("  exec < \"$prov_hook_stdin\"\n");
        block.push_str("  rm -f \"$prov_hook_stdin\" 2>/dev/null || :\n");
        block.push_str("fi\n");
    } else {
        block.push_str(&format!(
            "[ -x {binary} ] && {binary} git-event post-commit \"$@\" </dev/null >/dev/null 2>&1 &\n"
        ));
    }
    block.push_str("# <<< prov-hook <<<\n");
    block.into_bytes()
}

fn strip_verbatim_prefix(path: PathBuf) -> PathBuf {
    let value = path.to_string_lossy();
    if let Some(rest) = value.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    if let Some(rest) = value
        .strip_prefix(r"\\?\")
        .or_else(|| value.strip_prefix("//?/"))
    {
        return PathBuf::from(rest);
    }
    path
}

fn shell_quote(path: &Path) -> String {
    let path = strip_verbatim_prefix(path.to_path_buf());
    let value = path.to_string_lossy().replace('\\', "/");
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn shell_shebang_end(bytes: &[u8]) -> Option<(usize, bool)> {
    let end = bytes.iter().position(|byte| *byte == b'\n');
    let line_end = end.unwrap_or(bytes.len());
    let line = bytes[..line_end]
        .strip_suffix(b"\r")
        .unwrap_or(&bytes[..line_end]);
    let line = std::str::from_utf8(line).ok()?;
    let interpreter = line.strip_prefix("#!")?.trim();
    let tokens: Vec<_> = interpreter.split_ascii_whitespace().collect();
    let first = *tokens.first()?;
    let basename = Path::new(first).file_name()?.to_str()?;
    let shell = if basename == "env" {
        tokens
            .iter()
            .skip(1)
            .find(|token| !token.starts_with('-'))
            .and_then(|token| Path::new(token).file_name())
            .and_then(|name| name.to_str())?
    } else {
        basename
    };
    if shell != "sh" && shell != "bash" {
        return None;
    }
    Some((end.map_or(bytes.len(), |index| index + 1), end.is_some()))
}

fn has_marker(bytes: &[u8]) -> bool {
    contains(bytes, OPEN_MARKER) || contains(bytes, CLOSE_MARKER)
}

fn find_block(bytes: &[u8]) -> Option<HookBlock> {
    if count_matches(bytes, OPEN_MARKER) != 1 || count_matches(bytes, CLOSE_MARKER) != 1 {
        return None;
    }
    let start = find(bytes, OPEN_MARKER)?;
    if start > 0 && bytes[start - 1] != b'\n' {
        return None;
    }
    let close_start = find_from(bytes, CLOSE_MARKER, start + OPEN_MARKER.len())?;
    let close_after = close_start + CLOSE_MARKER.len();
    let end =
        if bytes.get(close_after) == Some(&b'\r') && bytes.get(close_after + 1) == Some(&b'\n') {
            close_after + 2
        } else if bytes.get(close_after) == Some(&b'\n') {
            close_after + 1
        } else {
            close_after
        };
    let content = &bytes[start..end];
    Some(HookBlock {
        start,
        end,
        created: contains(content, CREATED_MARKER),
        added_newline: contains(content, ADDED_NEWLINE_MARKER),
    })
}

fn find(bytes: &[u8], needle: &[u8]) -> Option<usize> {
    find_from(bytes, needle, 0)
}

fn find_from(bytes: &[u8], needle: &[u8], start: usize) -> Option<usize> {
    bytes[start..]
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|index| start + index)
}

fn count_matches(bytes: &[u8], needle: &[u8]) -> usize {
    bytes
        .windows(needle.len())
        .filter(|window| *window == needle)
        .count()
}

fn contains(bytes: &[u8], needle: &[u8]) -> bool {
    find(bytes, needle).is_some()
}

fn git_output(repo: &Path, args: &[&str]) -> Result<std::process::Output, String> {
    let mut command = Command::new("git");
    command
        .args(args)
        .current_dir(repo)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null());
    prov_capture::configure_process_command(&mut command, false);
    command.output().map_err(|error| error.to_string())
}
