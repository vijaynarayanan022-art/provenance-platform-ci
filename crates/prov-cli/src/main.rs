//! `prov analyze` - evidence-based AI attribution report for ONE commit or a PR range.
//!
//! Reads: `git diff-tree` (added lines), the Git AI note on the commit, and Git AI's raw
//! checkpoints archived on this machine. Writes nothing; every git call is read-only.

use prov_core::LineCounts;
use prov_gitai::{
    classify_added_lines_backed, classify_added_lines_explained, parse_blame_porcelain,
    parse_checkpoints_jsonl, parse_note, parse_unified_diff_added_lines, Backing, ExclusionPolicy,
    LineExplanation, MappingPolicy, NoteMeta, ParsedNote,
};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;

const USAGE: &str = "\
usage: prov analyze [--repo PATH] [--commit REV] [--base REV --head REV] [--json] [--explain]

  Single-commit mode (default):
    --repo    repository path (default: current directory)
    --commit  commit to analyze (default: HEAD)
    --json    machine-readable output

  PR range mode:
    --base    base revision (exclusive, e.g. the merge base)
    --head    head revision (inclusive, e.g. the PR tip)
    --json    machine-readable output
    --explain list per-file, per-line category and reason
";

fn git(repo: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["-c", "core.quotepath=false"])
        .args(args)
        .output()
        .map_err(|e| format!("cannot run git: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn pct(v: Option<f64>) -> String {
    v.map(|x| format!("{x:.1}%")).unwrap_or_else(|| "n/a".to_string())
}

fn find_checkpoints(git_dir: &Path, parent: Option<&str>) -> Option<PathBuf> {
    let key = parent.unwrap_or("initial");
    for dir in [format!("old-{key}"), key.to_string()] {
        let p = git_dir
            .join("ai")
            .join("working_logs")
            .join(dir)
            .join("checkpoints.jsonl");
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

/// Information gathered about a single commit in the PR range.
#[derive(Debug)]
struct CommitInfo {
    sha: String,
    subject: String,
    note_found: bool,
    checkpoints_found: bool,
    lines_attributed: u64,
}

fn run_single_commit(
    repo: &Path,
    commit: &str,
    json: bool,
) -> Result<(), String> {
    let sha = git(repo, &["rev-parse", "--verify", &format!("{commit}^{{commit}}")])?
        .trim()
        .to_string();
    let subject = git(repo, &["log", "-1", "--format=%s", &sha])?
        .trim()
        .to_string();
    let parents_line = git(repo, &["rev-list", "--parents", "-n", "1", &sha])?;
    let parents: Vec<&str> = parents_line.split_whitespace().skip(1).collect();
    if parents.len() > 1 {
        return Err("merge commits are not supported yet".to_string());
    }
    let parent = parents.first().copied();

    let diff = git(
        repo,
        &[
            "diff-tree",
            "-p",
            "-r",
            "--root",
            "--unified=0",
            "--no-color",
            "--no-ext-diff",
            &sha,
        ],
    )?;
    let added = parse_unified_diff_added_lines(&diff);

    let mut warnings: Vec<String> = Vec::new();
    let note_text = git(repo, &["notes", "--ref=ai", "show", &sha]).ok();
    let note_found = note_text.is_some();
    let note = match note_text {
        Some(t) => parse_note(&t).map_err(|e| format!("cannot read Git AI note: {e}"))?,
        None => {
            warnings.push("no Git AI note on this commit: every added line is unknown".into());
            ParsedNote {
                files: vec![],
                meta: NoteMeta::default(),
            }
        }
    };

    let git_dir = PathBuf::from(
        git(repo, &["rev-parse", "--absolute-git-dir"])?
            .trim(),
    );
    let cp_path = find_checkpoints(&git_dir, parent);
    let (backing, cp_count) = match &cp_path {
        Some(p) => {
            let text = std::fs::read_to_string(p)
                .map_err(|e| format!("cannot read {}: {e}", p.display()))?;
            let cps = parse_checkpoints_jsonl(&text)?;
            (Some(Backing::from_checkpoints(&cps)), cps.len())
        }
        None => {
            if note_found {
                warnings.push("raw Git AI checkpoints not found on this machine: AI claims in the note could NOT be verified".into());
            }
            (None, 0)
        }
    };

    let r = classify_added_lines_backed(
        &note,
        &added,
        &MappingPolicy::default(),
        &ExclusionPolicy::with_lockfiles(),
        backing.as_ref(),
    );
    let m = &r.metric;
    if r.unbacked_ai_lines > 0 {
        warnings.push(format!(
            "{} line(s) credited to AI by the note have no raw checkpoint behind them: counted as unknown",
            r.unbacked_ai_lines
        ));
    }

    if json {
        let v = serde_json::json!({
            "commit": sha, "subject": subject, "note_found": note_found,
            "raw_checkpoints": { "found": cp_path.is_some(), "path": cp_path.as_ref().map(|p| p.display().to_string()), "count": cp_count },
            "eligible_added_lines": m.eligible,
            "counts": m.counts,
            "ai_deterministic_pct": m.ai_deterministic_pct,
            "ai_linked_pct": m.ai_linked_pct,
            "known_human_lines": r.known_human_lines,
            "conflicting_lines": r.conflicting_lines,
            "unbacked_ai_lines": r.unbacked_ai_lines,
            "warnings": warnings,
        });
        println!("{}", serde_json::to_string_pretty(&v).unwrap());
    } else {
        let c = &m.counts;
        println!(
            "Commit {} - {}",
            &sha[..sha.len().min(10)],
            subject
        );
        println!(
            "Eligible added lines: {}   (excluded: {})",
            m.eligible, c.excluded
        );
        println!(
            "  ai_direct {}  ai_strong {}  ai_probable {}  mixed {}  unknown {}",
            c.direct, c.strong, c.probable, c.mixed, c.unknown
        );
        println!(
            "AI deterministic: {}    AI linked: {}",
            pct(m.ai_deterministic_pct),
            pct(m.ai_linked_pct)
        );
        println!(
            "Human-signal lines (counted as unknown): {}   Conflicts: {}   Unbacked AI claims: {}",
            r.known_human_lines, r.conflicting_lines, r.unbacked_ai_lines
        );
        match &cp_path {
            Some(p) => println!(
                "Raw checkpoints: {} record(s) from {}",
                cp_count,
                p.display()
            ),
            None => println!("Raw checkpoints: not found"),
        }
        for w in &warnings {
            println!("WARNING: {w}");
        }
    }
    Ok(())
}

fn run_pr_range(
    repo: &Path,
    base_rev: &str,
    head_rev: &str,
    json: bool,
    explain: bool,
) -> Result<(), String> {
    // Resolve revisions
    let base_sha = git(
        repo,
        &["rev-parse", "--verify", &format!("{base_rev}^{{commit}}")],
    )?
    .trim()
    .to_string();
    let head_sha = git(
        repo,
        &["rev-parse", "--verify", &format!("{head_rev}^{{commit}}")],
    )?
    .trim()
    .to_string();

    // List commits in base..head (oldest first)
    let rev_list = git(
        repo,
        &["rev-list", "--reverse", &format!("{base_sha}..{head_sha}")],
    )?;
    let commits: Vec<&str> = rev_list.lines().filter(|l| !l.is_empty()).collect();

    // Check for merge commits in the range
    for &c in &commits {
        let parents_line = git(repo, &["rev-list", "--parents", "-n", "1", c])?;
        let parents: Vec<&str> = parents_line.split_whitespace().skip(1).collect();
        if parents.len() > 1 {
            return Err(format!(
                "merge commit {} found in range: merge commits are not supported yet",
                &c[..c.len().min(10)]
            ));
        }
    }

    let commit_set: HashSet<String> = commits.iter().map(|s| s.to_string()).collect();

    // Step 1: Get the overall diff base..head -> added lines in head version
    let diff = git(
        repo,
        &[
            "diff",
            "--unified=0",
            "--no-color",
            "--no-ext-diff",
            &format!("{base_sha}..{head_sha}"),
        ],
    )?;
    let added_in_head = parse_unified_diff_added_lines(&diff);

    // Step 2: For each file with added lines, run git blame on head to find introducing commit
    // and original line numbers.
    // per_commit_lines: commit_sha -> (file_path -> set of original line numbers in that commit)
    let mut per_commit_lines: HashMap<String, BTreeMap<String, BTreeSet<u32>>> = HashMap::new();
    let mut orig_to_final: HashMap<(String, String, u32), u32> = HashMap::new();
    let mut _total_skipped = 0u64;

    for (file_path, line_numbers) in &added_in_head {
        let blame_output = git(
            repo,
            &[
                "blame",
                "--line-porcelain",
                &head_sha,
                "--",
                file_path,
            ],
        )?;
        let blame_map = parse_blame_porcelain(&blame_output);

        for &line in line_numbers {
            if let Some(bl) = blame_map.get(&line) {
                if commit_set.contains(&bl.commit) {
                    let normed = file_path.replace('\\', "/");
                    per_commit_lines
                        .entry(bl.commit.clone())
                        .or_default()
                        .entry(normed.clone())
                        .or_default()
                        .insert(bl.orig_line);
                    orig_to_final.insert(
                        (bl.commit.clone(), normed, bl.orig_line),
                        line,
                    );
                } else {
                    // Line was introduced by a commit outside the range (e.g. a base line
                    // that shows up in the diff due to context). Skip it.
                    _total_skipped += 1;
                }
            }
            // If blame doesn't have this line, something unexpected happened; skip silently.
        }
    }

    // Step 3: For each commit, load its note + checkpoints and classify using ORIGINAL line numbers.
    let git_dir = PathBuf::from(
        git(repo, &["rev-parse", "--absolute-git-dir"])?
            .trim(),
    );
    let mapping = MappingPolicy::default();
    let exclusion = ExclusionPolicy::with_lockfiles();
    let mut total_counts = LineCounts::default();
    let mut total_known_human = 0u64;
    let mut total_conflicts = 0u64;
    let mut total_unbacked = 0u64;
    let mut warnings: Vec<String> = Vec::new();
    let mut commit_infos: Vec<CommitInfo> = Vec::new();
    let mut all_explanations: Vec<LineExplanation> = Vec::new();

    for &commit_sha in &commits {
        let subject = git(repo, &["log", "-1", "--format=%s", commit_sha])?
            .trim()
            .to_string();
        let parents_line = git(repo, &["rev-list", "--parents", "-n", "1", commit_sha])?;
        let parent = parents_line
            .split_whitespace()
            .nth(1)
            .map(|s| s.to_string());

        let added_for_commit = per_commit_lines
            .remove(commit_sha)
            .unwrap_or_default();

        let commit_line_count: u64 = added_for_commit.values().map(|s| s.len() as u64).sum();

        // Load note
        let note_text = git(repo, &["notes", "--ref=ai", "show", commit_sha]).ok();
        let note_found = note_text.is_some();
        let note = match note_text {
            Some(t) => parse_note(&t).map_err(|e| format!("cannot read Git AI note on {}: {e}", &commit_sha[..10.min(commit_sha.len())]))?,
            None => ParsedNote {
                files: vec![],
                meta: NoteMeta::default(),
            },
        };

        // Load checkpoints
        let cp_path = find_checkpoints(&git_dir, parent.as_deref());
        let (backing, _cp_count) = match &cp_path {
            Some(p) => {
                let text = std::fs::read_to_string(p)
                    .map_err(|e| format!("cannot read {}: {e}", p.display()))?;
                let cps = parse_checkpoints_jsonl(&text)?;
                (Some(Backing::from_checkpoints(&cps)), cps.len())
            }
            None => {
                if note_found && commit_line_count > 0 {
                    warnings.push(format!(
                        "commit {}: raw checkpoints not found; AI claims could NOT be verified",
                        &commit_sha[..commit_sha.len().min(10)]
                    ));
                }
                (None, 0)
            }
        };

        let checkpoints_found = cp_path.is_some();

        // Classify
        let (r, mut expl) = if explain {
            classify_added_lines_explained(
                &note,
                &added_for_commit,
                &mapping,
                &exclusion,
                backing.as_ref(),
            )
        } else {
            let r = classify_added_lines_backed(
                &note,
                &added_for_commit,
                &mapping,
                &exclusion,
                backing.as_ref(),
            );
            (r, Vec::new())
        };

        // Merge counts
        total_counts.direct += r.metric.counts.direct;
        total_counts.strong += r.metric.counts.strong;
        total_counts.probable += r.metric.counts.probable;
        total_counts.mixed += r.metric.counts.mixed;
        total_counts.unknown += r.metric.counts.unknown;
        total_counts.excluded += r.metric.counts.excluded;
        total_known_human += r.known_human_lines;
        total_conflicts += r.conflicting_lines;
        total_unbacked += r.unbacked_ai_lines;
        if r.unbacked_ai_lines > 0 {
            warnings.push(format!(
                "commit {}: {} unbacked AI claim(s) counted as unknown",
                &commit_sha[..commit_sha.len().min(10)],
                r.unbacked_ai_lines
            ));
        }

        if explain {
            for e in &mut expl {
                e.commit = Some(commit_sha.to_string());
                let key = (commit_sha.to_string(), e.file.clone(), e.line);
                if let Some(&final_l) = orig_to_final.get(&key) {
                    e.line = final_l;
                }
                if !note_found && e.reason == "no note entry -> unknown" {
                    e.reason = "no note -> unknown".to_string();
                }
            }
            all_explanations.append(&mut expl);
        }

        commit_infos.push(CommitInfo {
            sha: commit_sha.to_string(),
            subject,
            note_found,
            checkpoints_found,
            lines_attributed: commit_line_count,
        });

        // Warn about missing notes
        if !note_found && commit_line_count > 0 {
            warnings.push(format!(
                "commit {}: no Git AI note; {} line(s) from this commit are classified as unknown",
                &commit_sha[..commit_sha.len().min(10)],
                commit_line_count
            ));
        }
    }

    all_explanations.sort_by(|a, b| (&a.file, a.line).cmp(&(&b.file, b.line)));

    // Compute merged metric
    let metric = prov_core::PrMetric::from_counts(total_counts);

    if json {
        let commits_json: Vec<serde_json::Value> = commit_infos
            .iter()
            .map(|ci| {
                serde_json::json!({
                    "sha": ci.sha,
                    "subject": ci.subject,
                    "note_found": ci.note_found,
                    "checkpoints_found": ci.checkpoints_found,
                    "lines_in_final": ci.lines_attributed,
                })
            })
            .collect();

        let mut v = serde_json::json!({
            "mode": "pr_range",
            "base": base_sha,
            "head": head_sha,
            "commits_in_range": commits.len(),
            "eligible_added_lines": metric.eligible,
            "counts": metric.counts,
            "ai_deterministic_pct": metric.ai_deterministic_pct,
            "ai_linked_pct": metric.ai_linked_pct,
            "known_human_lines": total_known_human,
            "conflicting_lines": total_conflicts,
            "unbacked_ai_lines": total_unbacked,
            "per_commit": commits_json,
            "warnings": warnings,
        });

        if explain {
            v["explanations"] = serde_json::to_value(&all_explanations).unwrap();
        }

        println!("{}", serde_json::to_string_pretty(&v).unwrap());
    } else {
        println!(
            "PR range: {}..{} ({} commit(s))",
            &base_sha[..base_sha.len().min(10)],
            &head_sha[..head_sha.len().min(10)],
            commits.len()
        );
        let c = &metric.counts;
        println!(
            "Eligible added lines: {}   (excluded: {})",
            metric.eligible, c.excluded
        );
        println!(
            "  ai_direct {}  ai_strong {}  ai_probable {}  mixed {}  unknown {}",
            c.direct, c.strong, c.probable, c.mixed, c.unknown
        );
        println!(
            "AI deterministic: {}    AI linked: {}",
            pct(metric.ai_deterministic_pct),
            pct(metric.ai_linked_pct)
        );
        println!(
            "Human-signal lines (counted as unknown): {}   Conflicts: {}   Unbacked AI claims: {}",
            total_known_human, total_conflicts, total_unbacked
        );
        println!();
        for ci in &commit_infos {
            println!(
                "  {} {} | {} line(s) | note:{} checkpoints:{}",
                &ci.sha[..ci.sha.len().min(10)],
                ci.subject,
                ci.lines_attributed,
                if ci.note_found { "yes" } else { "no" },
                if ci.checkpoints_found { "yes" } else { "no" },
            );
        }

        if explain {
            println!();
            println!("--- Per-line explanations ---");
            // Group by file
            let mut by_file: BTreeMap<String, Vec<&LineExplanation>> = BTreeMap::new();
            for e in &all_explanations {
                by_file.entry(e.file.clone()).or_default().push(e);
            }
            for (file, mut lines) in by_file {
                println!("  {}:", file);
                lines.sort_by_key(|e| e.line);
                for e in lines {
                    let commit_str = match &e.commit {
                        Some(c) => format!(" [{}]", &c[..c.len().min(10)]),
                        None => String::new(),
                    };
                    println!(
                        "    L{}: {}{} - {}",
                        e.line, e.category, commit_str, e.reason
                    );
                }
            }
        }

        for w in &warnings {
            println!("WARNING: {w}");
        }
    }

    Ok(())
}

fn run(args: &[String]) -> Result<(), String> {
    if args.first().map(String::as_str) != Some("analyze") {
        return Err(format!("usage\n{USAGE}"));
    }
    let (mut repo, mut commit, mut json, mut explain) =
        (PathBuf::from("."), "HEAD".to_string(), false, false);
    let (mut base, mut head): (Option<String>, Option<String>) = (None, None);
    let mut commit_specified = false;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--repo" => {
                i += 1;
                repo = PathBuf::from(
                    args.get(i).ok_or("usage\n--repo needs a value")?,
                );
            }
            "--commit" => {
                i += 1;
                commit = args
                    .get(i)
                    .ok_or("usage\n--commit needs a value")?
                    .clone();
                commit_specified = true;
            }
            "--base" => {
                i += 1;
                base = Some(
                    args.get(i)
                        .ok_or("usage\n--base needs a value")?
                        .clone(),
                );
            }
            "--head" => {
                i += 1;
                head = Some(
                    args.get(i)
                        .ok_or("usage\n--head needs a value")?
                        .clone(),
                );
            }
            "--json" => json = true,
            "--explain" => explain = true,
            other => {
                return Err(format!(
                    "usage\nunknown argument {other:?}\n{USAGE}"
                ))
            }
        }
        i += 1;
    }

    match (base, head) {
        (Some(b), Some(h)) => {
            if commit_specified {
                return Err("usage\n--commit cannot be combined with --base and --head".to_string());
            }
            run_pr_range(&repo, &b, &h, json, explain)
        }
        (None, None) => {
            if explain {
                return Err(
                    "usage\n--explain requires --base and --head (PR range mode)".to_string(),
                );
            }
            run_single_commit(&repo, &commit, json)
        }
        _ => Err("usage\n--base and --head must both be specified for PR range mode".to_string()),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => {}
        Err(e) => {
            if let Some(u) = e.strip_prefix("usage\n") {
                eprintln!("{u}");
                std::process::exit(2);
            }
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    }
}
