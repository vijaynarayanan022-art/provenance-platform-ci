use std::io::Read;
use std::panic;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

mod setup;

fn main() -> ExitCode {
    panic::set_hook(Box::new(|_| {}));
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("codex") => run_codex(),
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
        _ => run_codex(),
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
    let mut binary = None;
    let mut dry_run = false;
    let mut enable_hooks_feature = false;
    let mut args = args;
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--codex-home" => {
                codex_home = Some(PathBuf::from(
                    args.next().ok_or("--codex-home needs a path")?,
                ))
            }
            "--binary" => binary = Some(PathBuf::from(args.next().ok_or("--binary needs a path")?)),
            "--dry-run" => dry_run = true,
            "--enable-hooks-feature" => enable_hooks_feature = true,
            _ => return Err(format!("unknown install argument: {argument}")),
        }
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
    let mut dry_run = false;
    let mut args = args;
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--codex-home" => {
                codex_home = Some(PathBuf::from(
                    args.next().ok_or("--codex-home needs a path")?,
                ))
            }
            "--dry-run" => dry_run = true,
            _ => return Err(format!("unknown uninstall argument: {argument}")),
        }
    }
    setup::uninstall(codex_home.as_deref(), dry_run)
}

fn doctor_command(args: impl Iterator<Item = String>) -> Result<bool, String> {
    let mut codex_home = None;
    let mut repo = None;
    let mut args = args;
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--codex-home" => {
                codex_home = Some(PathBuf::from(
                    args.next().ok_or("--codex-home needs a path")?,
                ))
            }
            "--repo" => repo = Some(PathBuf::from(args.next().ok_or("--repo needs a path")?)),
            _ => return Err(format!("unknown doctor argument: {argument}")),
        }
    }
    let report = setup::doctor(codex_home.as_deref(), repo.as_deref())?;
    for line in report.lines {
        println!("{line}");
    }
    Ok(report.failed)
}
