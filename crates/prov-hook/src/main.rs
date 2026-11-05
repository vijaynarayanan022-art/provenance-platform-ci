use std::io::Read;
use std::panic;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

mod git_hooks;
mod pr_report;
mod setup;
mod vscode_setup;

fn main() -> ExitCode {
    panic::set_hook(Box::new(|_| {}));
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("codex") => run_codex(),
        Some("vscode") => run_vscode(),
        Some("derive") => {
            let args: Vec<_> = args.collect();
            if args.iter().any(|argument| argument == "--pending") {
                let _ = panic::catch_unwind(|| derive_pending_command(&args));
                ExitCode::SUCCESS
            } else {
                match derive_command(args.into_iter()) {
                    Ok(commits) => {
                        for commit in commits {
                            println!("derived {commit}");
                        }
                        ExitCode::SUCCESS
                    }
                    Err(error) => cli_error(error),
                }
            }
        }
        Some("show") => match show_command(args) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => cli_error(error),
        },
        Some("pr-report") => match pr_report::command(args) {
            Ok((bytes, output_path)) => {
                let result = if let Some(path) = output_path {
                    std::fs::write(path, bytes)
                } else {
                    use std::io::Write;
                    std::io::stdout().write_all(&bytes)
                };
                match result {
                    Ok(()) => ExitCode::SUCCESS,
                    Err(error) => cli_error(error.to_string()),
                }
            }
            Err(error) => cli_error(error),
        },
        Some("install") => match install_command(args) {
            Ok(output) => {
                print!("{output}");
                ExitCode::SUCCESS
            }
            Err(error) => cli_error(error),
        },
        Some("uninstall") => match uninstall_command(args) {
            Ok(output) => {
                print!("{output}");
                ExitCode::SUCCESS
            }
            Err(error) => cli_error(error),
        },
        Some("doctor") => match doctor_command(args) {
            Ok(failed) => {
                if failed {
                    ExitCode::FAILURE
                } else {
                    ExitCode::SUCCESS
                }
            }
            Err(error) => cli_error(error),
        },
        Some("setup") => match vscode_setup_command(args) {
            Ok(output) => {
                print!("{output}");
                ExitCode::SUCCESS
            }
            Err(error) => cli_error(error),
        },
        Some("git-event") => git_event_command(args),
        Some("install-git") => match install_git_command(args) {
            Ok((output, _unsupported)) => {
                print!("{output}");
                ExitCode::SUCCESS
            }
            Err(error) => cli_error(error),
        },
        Some("uninstall-git") => match uninstall_git_command(args) {
            Ok(output) => {
                print!("{output}");
                ExitCode::SUCCESS
            }
            Err(error) => cli_error(error),
        },
        _ => run_codex(),
    }
}

fn git_event_command(args: impl Iterator<Item = String>) -> ExitCode {
    let watchdog = prov_capture::start_watchdog(Duration::from_millis(1400));
    let args: Vec<_> = args.collect();
    let _ = panic::catch_unwind(|| git_event_inner(args));
    watchdog.finish();
    ExitCode::SUCCESS
}

fn git_event_inner(args: Vec<String>) {
    let Some(event) = args.first().map(String::as_str) else {
        return;
    };
    if event != "post-commit" && event != "post-rewrite" {
        return;
    }
    let mut repo = std::env::current_dir().unwrap_or_default();
    let mut args = args.iter().skip(1);
    while let Some(argument) = args.next() {
        if argument == "--repo" {
            let Some(path) = args.next() else {
                return;
            };
            repo = PathBuf::from(path);
        }
    }
    let Ok(repo) = git_hooks::repo_root(&repo) else {
        return;
    };
    let commits = if event == "post-commit" {
        let Ok(commit) = git_hooks::head_commit(&repo) else {
            return;
        };
        vec![commit]
    } else {
        let mut input = Vec::new();
        let _ = std::io::stdin().take(256 * 1024).read_to_end(&mut input);
        input
            .split(|byte| *byte == b'\n')
            .take(1000)
            .filter_map(|line| {
                let line = std::str::from_utf8(line).ok()?.trim();
                if line.is_empty() {
                    return None;
                }
                let mut fields = line.split_whitespace();
                let old = fields.next()?;
                let new = fields.next()?;
                let _ = old;
                if new.len() != 40 && new.len() != 64 {
                    return None;
                }
                if !new.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                    return None;
                }
                Some(new.to_owned())
            })
            .collect()
    };
    if prov_capture::commit::enqueue_git_commits(&repo, &commits).unwrap_or(false) {
        prov_capture::spawn_pending_derive(&repo);
    }
}

fn run_codex() -> ExitCode {
    let result = panic::catch_unwind(|| {
        let watchdog = prov_capture::start_watchdog(Duration::from_millis(1400));
        let mut input = Vec::new();
        let _ = std::io::stdin()
            .take(16 * 1024 * 1024)
            .read_to_end(&mut input);
        prov_capture::process_json(&input);
        watchdog.finish();
    });
    let _ = result;
    ExitCode::SUCCESS
}

fn run_vscode() -> ExitCode {
    let watchdog = prov_capture::start_watchdog_with_output(Duration::from_millis(1400), b"{}");
    let _ = panic::catch_unwind(|| {
        let mut input = Vec::new();
        let _ = std::io::stdin()
            .take(16 * 1024 * 1024)
            .read_to_end(&mut input);
        prov_capture::process_vscode_json(&input);
    });
    if watchdog.finish() {
        use std::io::Write;
        let mut stdout = std::io::stdout();
        let _ = stdout.write_all(b"{}");
        let _ = stdout.flush();
    }
    ExitCode::SUCCESS
}

fn derive_command(args: impl Iterator<Item = String>) -> Result<Vec<String>, String> {
    let mut repo = std::env::current_dir().map_err(|error| error.to_string())?;
    let mut commit = None;
    let mut range = None;
    let mut args = args;
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--repo" => repo = PathBuf::from(args.next().ok_or("--repo needs a path")?),
            "--commit" => commit = Some(args.next().ok_or("--commit needs a revision")?),
            "--range" => range = Some(args.next().ok_or("--range needs A..B")?),
            _ => return Err(format!("unknown derive argument: {argument}")),
        }
    }
    if commit.is_some() && range.is_some() {
        return Err("--commit and --range are mutually exclusive".into());
    }
    let paths = if let Some(range) = range {
        prov_capture::commit::derive_range(&repo, &range)?
    } else {
        vec![prov_capture::commit::derive_commit(
            &repo,
            commit.as_deref().unwrap_or("HEAD"),
        )?]
    };
    Ok(paths
        .iter()
        .filter_map(|path| {
            path.file_stem()
                .and_then(|stem| stem.to_str())
                .map(str::to_owned)
        })
        .collect())
}

fn derive_pending_command(args: &[String]) {
    let mut repo = std::env::current_dir().unwrap_or_default();
    let mut args = args.iter();
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--pending" => {}
            "--repo" => {
                let Some(path) = args.next() else {
                    return;
                };
                repo = PathBuf::from(path);
            }
            _ => return,
        }
    }
    let _ = prov_capture::commit::derive_pending(&repo);
}

fn show_command(args: impl Iterator<Item = String>) -> Result<(), String> {
    let mut repo = std::env::current_dir().map_err(|error| error.to_string())?;
    let mut commit = "HEAD".to_owned();
    let mut json = false;
    let mut args = args;
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--repo" => repo = PathBuf::from(args.next().ok_or("--repo needs a path")?),
            "--commit" => commit = args.next().ok_or("--commit needs a revision")?,
            "--json" => json = true,
            _ => return Err(format!("unknown show argument: {argument}")),
        }
    }
    let bytes = prov_capture::commit::show_commit(&repo, &commit)?;
    if json {
        use std::io::Write;
        std::io::stdout()
            .write_all(&bytes)
            .map_err(|error| error.to_string())?;
    } else {
        println!("{}", prov_capture::commit::summary(&bytes)?);
    }
    Ok(())
}

fn cli_error(error: String) -> ExitCode {
    eprintln!("{error}");
    ExitCode::FAILURE
}

fn install_command(args: impl Iterator<Item = String>) -> Result<String, String> {
    let mut codex_home = None;
    let mut copilot_home = None;
    let mut binary = None;
    let mut tool = "codex".to_owned();
    let mut dry_run = false;
    let mut enable_hooks_feature = false;
    let mut args = args;
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--tool" => tool = args.next().ok_or("--tool needs a value")?,
            "--codex-home" => {
                codex_home = Some(PathBuf::from(
                    args.next().ok_or("--codex-home needs a path")?,
                ))
            }
            "--copilot-home" => {
                copilot_home = Some(PathBuf::from(
                    args.next().ok_or("--copilot-home needs a path")?,
                ))
            }
            "--binary" => binary = Some(PathBuf::from(args.next().ok_or("--binary needs a path")?)),
            "--dry-run" => dry_run = true,
            "--enable-hooks-feature" => enable_hooks_feature = true,
            _ => return Err(format!("unknown install argument: {argument}")),
        }
    }
    if tool == "vscode" {
        if codex_home.is_some() || enable_hooks_feature {
            return Err("--codex-home and --enable-hooks-feature are Codex-only".into());
        }
        return vscode_setup::install(copilot_home.as_deref(), binary.as_deref(), dry_run);
    }
    if tool != "codex" || copilot_home.is_some() {
        return Err("--tool must be codex or vscode".into());
    }
    setup::install(
        codex_home.as_deref(),
        binary.as_deref(),
        dry_run,
        enable_hooks_feature,
    )
}

fn uninstall_command(args: impl Iterator<Item = String>) -> Result<String, String> {
    let mut codex_home = None;
    let mut copilot_home = None;
    let mut tool = "codex".to_owned();
    let mut dry_run = false;
    let mut args = args;
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--tool" => tool = args.next().ok_or("--tool needs a value")?,
            "--codex-home" => {
                codex_home = Some(PathBuf::from(
                    args.next().ok_or("--codex-home needs a path")?,
                ))
            }
            "--copilot-home" => {
                copilot_home = Some(PathBuf::from(
                    args.next().ok_or("--copilot-home needs a path")?,
                ))
            }
            "--dry-run" => dry_run = true,
            _ => return Err(format!("unknown uninstall argument: {argument}")),
        }
    }
    if tool == "vscode" {
        if codex_home.is_some() {
            return Err("--codex-home is Codex-only".into());
        }
        return vscode_setup::uninstall(copilot_home.as_deref(), dry_run);
    }
    if tool != "codex" || copilot_home.is_some() {
        return Err("--tool must be codex or vscode".into());
    }
    setup::uninstall(codex_home.as_deref(), dry_run)
}

fn vscode_setup_command(args: impl Iterator<Item = String>) -> Result<String, String> {
    let mut tool = None;
    let mut copilot_home = None;
    let mut binary = None;
    let mut args = args;
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--tool" => tool = Some(args.next().ok_or("--tool needs a value")?),
            "--copilot-home" => {
                copilot_home = Some(PathBuf::from(
                    args.next().ok_or("--copilot-home needs a path")?,
                ))
            }
            "--binary" => binary = Some(PathBuf::from(args.next().ok_or("--binary needs a path")?)),
            _ => return Err(format!("unknown setup argument: {argument}")),
        }
    }
    if tool.as_deref() != Some("vscode") {
        return Err("setup requires --tool vscode".into());
    }
    vscode_setup::install(copilot_home.as_deref(), binary.as_deref(), false)
}

fn install_git_command(args: impl Iterator<Item = String>) -> Result<(String, bool), String> {
    let mut repo = None;
    let mut args = args;
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--repo" => repo = Some(PathBuf::from(args.next().ok_or("--repo needs a path")?)),
            _ => return Err(format!("unknown install-git argument: {argument}")),
        }
    }
    let repo = repo.ok_or("install-git requires --repo")?;
    git_hooks::install(&repo)
}

fn uninstall_git_command(args: impl Iterator<Item = String>) -> Result<String, String> {
    let mut repo = None;
    let mut args = args;
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--repo" => repo = Some(PathBuf::from(args.next().ok_or("--repo needs a path")?)),
            _ => return Err(format!("unknown uninstall-git argument: {argument}")),
        }
    }
    let repo = repo.ok_or("uninstall-git requires --repo")?;
    git_hooks::uninstall(&repo)
}

fn doctor_command(args: impl Iterator<Item = String>) -> Result<bool, String> {
    let mut codex_home = None;
    let mut copilot_home = None;
    let mut tool = "codex".to_owned();
    let mut repo = None;
    let mut args = args;
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--tool" => tool = args.next().ok_or("--tool needs a value")?,
            "--codex-home" => {
                codex_home = Some(PathBuf::from(
                    args.next().ok_or("--codex-home needs a path")?,
                ))
            }
            "--copilot-home" => {
                copilot_home = Some(PathBuf::from(
                    args.next().ok_or("--copilot-home needs a path")?,
                ))
            }
            "--repo" => repo = Some(PathBuf::from(args.next().ok_or("--repo needs a path")?)),
            _ => return Err(format!("unknown doctor argument: {argument}")),
        }
    }
    if tool == "vscode" {
        if codex_home.is_some() || repo.is_some() {
            return Err("--codex-home and --repo are Codex-only".into());
        }
        let report = vscode_setup::doctor(copilot_home.as_deref())?;
        for line in report.lines {
            println!("{line}");
        }
        return Ok(report.failed);
    }
    if tool != "codex" || copilot_home.is_some() {
        return Err("--tool must be codex or vscode".into());
    }
    let report = setup::doctor(codex_home.as_deref(), repo.as_deref())?;
    for line in report.lines {
        println!("{line}");
    }
    Ok(report.failed)
}
