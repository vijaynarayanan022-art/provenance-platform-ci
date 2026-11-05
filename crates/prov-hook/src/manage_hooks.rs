use std::path::{Path, PathBuf};

struct Options {
    repo: Option<PathBuf>,
    codex: bool,
    vscode: bool,
    dry_run: bool,
}

pub fn command(
    args: impl Iterator<Item = String>,
    enabling: bool,
) -> Result<(String, bool), String> {
    let options = parse_options(args)?;
    let mut output = String::new();
    let mut failed = false;
    let (repo, repo_problem) = resolve_repo(options.repo.as_deref());

    let codex_path = setup::config_path(None);
    if options.codex {
        let result = if enabling {
            setup::install(None, None, options.dry_run, true)
        } else {
            setup::uninstall(None, options.dry_run)
        };
        append_result(
            &mut output,
            &mut failed,
            "codex",
            codex_path.as_ref().ok().map(|path| path.as_path()),
            result.map(|_| ()),
            options.dry_run,
            if enabling { "install" } else { "remove" },
        );
    } else {
        append_skipped(
            &mut output,
            "codex",
            codex_path.as_ref().ok().map(|path| path.as_path()),
            "not selected in --tools",
        );
    }

    let vscode_path = vscode_setup::hooks_file_path(None);
    if options.vscode {
        let result = if enabling {
            vscode_setup::install(None, None, options.dry_run)
        } else {
            vscode_setup::uninstall(None, options.dry_run)
        };
        append_result(
            &mut output,
            &mut failed,
            "vscode",
            vscode_path.as_ref().ok().map(|path| path.as_path()),
            result.map(|_| ()),
            options.dry_run,
            if enabling { "install" } else { "remove" },
        );
    } else {
        append_skipped(
            &mut output,
            "vscode",
            vscode_path.as_ref().ok().map(|path| path.as_path()),
            "not selected in --tools",
        );
    }

    match repo.as_ref() {
        Some(repo) => match git_hooks::managed_hook_paths(repo) {
            Ok(paths) if options.dry_run => {
                output.push_str(&format!(
                    "PASS git: would {} {}\n",
                    if enabling { "write" } else { "remove" },
                    display_paths(&paths)
                ));
            }
            Ok(paths) => {
                let result = if enabling {
                    git_hooks::install(repo).and_then(|(text, unsupported)| {
                        if unsupported {
                            Err(text.trim().to_owned())
                        } else {
                            Ok(text)
                        }
                    })
                } else {
                    git_hooks::uninstall(repo)
                };
                append_result_paths(&mut output, &mut failed, "git", &paths, result);
            }
            Err(error) => {
                failed = true;
                output.push_str(&format!("FAIL git: <unresolved> ({error})\n"));
            }
        },
        None if options.repo.is_some() => {
            failed = true;
            output.push_str(&format!(
                "FAIL git: {} ({})\n",
                options.repo.as_deref().unwrap().display(),
                repo_problem.as_deref().unwrap_or("not a Git repository")
            ));
        }
        None => output
            .push_str("SKIPPED git: <no repository> (no Git repository in current directory)\n"),
    }

    if enabling {
        output.push_str("DOCTOR codex:\n");
        match setup::doctor(None, repo.as_deref()) {
            Ok(report) => {
                for line in report.lines {
                    output.push_str(&line);
                    output.push('\n');
                }
            }
            Err(error) => output.push_str(&format!("FAIL doctor codex: {error}\n")),
        }
        output.push_str("DOCTOR vscode:\n");
        match vscode_setup::doctor(None) {
            Ok(report) => {
                for line in report.lines {
                    output.push_str(&line);
                    output.push('\n');
                }
            }
            Err(error) => output.push_str(&format!("FAIL doctor vscode: {error}\n")),
        }
    }
    Ok((output, failed))
}

fn parse_options(args: impl Iterator<Item = String>) -> Result<Options, String> {
    let mut repo = None;
    let mut tools = None;
    let mut dry_run = false;
    let mut args = args;
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--repo" => repo = Some(PathBuf::from(args.next().ok_or("--repo needs a path")?)),
            "--tools" => tools = Some(args.next().ok_or("--tools needs a value")?),
            "--dry-run" => dry_run = true,
            _ => return Err(format!("unknown hook-management argument: {argument}")),
        }
    }
    let (codex, vscode) = match tools.as_deref().unwrap_or("codex,vscode") {
        "" => return Err("--tools must include codex and/or vscode".into()),
        value => {
            let mut codex = false;
            let mut vscode = false;
            for tool in value.split(',') {
                match tool {
                    "codex" => codex = true,
                    "vscode" => vscode = true,
                    _ => return Err(format!("unsupported tool in --tools: {tool}")),
                }
            }
            (codex, vscode)
        }
    };
    if !codex && !vscode {
        return Err("--tools must include codex and/or vscode".into());
    }
    Ok(Options {
        repo,
        codex,
        vscode,
        dry_run,
    })
}

fn resolve_repo(requested: Option<&Path>) -> (Option<PathBuf>, Option<String>) {
    if let Some(path) = requested {
        return match git_hooks::repo_root(path) {
            Ok(repo) => (Some(repo), None),
            Err(error) => (None, Some(error)),
        };
    }
    let cwd = std::env::current_dir().ok();
    match cwd.as_deref().map(git_hooks::repo_root) {
        Some(Ok(repo)) => (Some(repo), None),
        Some(Err(error)) => (None, Some(error)),
        None => (None, Some("could not determine current directory".into())),
    }
}

fn append_result(
    output: &mut String,
    failed: &mut bool,
    name: &str,
    path: Option<&Path>,
    result: Result<(), String>,
    dry_run: bool,
    dry_run_action: &str,
) {
    let path = path.map_or_else(|| "<unresolved>".into(), |path| path.display().to_string());
    match result {
        Ok(()) => output.push_str(&format!(
            "PASS {name}: {}{path}\n",
            if dry_run {
                format!("would {dry_run_action} ")
            } else {
                String::new()
            }
        )),
        Err(error) => {
            *failed = true;
            output.push_str(&format!("FAIL {name}: {path} ({error})\n"));
        }
    }
}

fn append_result_paths(
    output: &mut String,
    failed: &mut bool,
    name: &str,
    paths: &[PathBuf],
    result: Result<String, String>,
) {
    let paths = display_paths(paths);
    match result {
        Ok(_) => output.push_str(&format!("PASS {name}: {paths}\n")),
        Err(error) => {
            *failed = true;
            output.push_str(&format!("FAIL {name}: {paths} ({error})\n"));
        }
    }
}

fn append_skipped(output: &mut String, name: &str, path: Option<&Path>, reason: &str) {
    let path = path.map_or_else(|| "<unresolved>".into(), |path| path.display().to_string());
    output.push_str(&format!("SKIPPED {name}: {path} ({reason})\n"));
}

fn display_paths(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

use crate::{git_hooks, setup, vscode_setup};
