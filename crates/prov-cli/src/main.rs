//! `prov analyze` - evidence-based AI attribution report for ONE commit or a PR range.
//!
//! Reads: `git diff-tree` (added lines), the Git AI note on the commit, and Git AI's raw
//! checkpoints archived on this machine. Writes nothing; every git call is read-only.

use prov_core::{Category, LineCounts};
use prov_gitai::{
    classify_added_lines_backed, classify_added_lines_explained, parse_blame_porcelain,
    parse_checkpoints_jsonl, parse_note, parse_unified_diff_added_lines, Backing, BlameLine,
    ExclusionPolicy, LineExplanation, MappingPolicy, NoteMeta, ParsedNote, ReasonCode,
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

/// Evidence loaded for one commit. Parse failures never abort the analysis.
#[derive(Debug)]
struct Evidence {
    note: ParsedNote,
    note_found: bool,
    note_error: Option<String>,
    backing: Option<Backing>,
    cp_path: Option<PathBuf>,
    cp_count: usize,
    cp_error: Option<String>,
}

fn empty_note() -> ParsedNote {
    ParsedNote {
        files: vec![],
        meta: NoteMeta::default(),
    }
}

fn load_evidence(repo: &Path, git_dir: &Path, sha: &str, parent: Option<&str>) -> Evidence {
    let note_text = git(repo, &["notes", "--ref=ai", "show", sha]).ok();
    let (note, note_found, note_error) = match note_text {
        Some(t) => match parse_note(&t) {
            Ok(n) => (n, true, None),
            Err(e) => (empty_note(), false, Some(e.to_string())),
        },
        None => (empty_note(), false, None),
    };

    let cp_path = find_checkpoints(git_dir, parent);
    let (backing, cp_count, cp_error) = match &cp_path {
        Some(p) => match std::fs::read_to_string(p) {
            Ok(text) => match parse_checkpoints_jsonl(&text) {
                Ok(cps) => {
                    let n = cps.len();
                    (Some(Backing::from_checkpoints(&cps)), n, None)
                }
                Err(e) => (None, 0, Some(e.to_string())),
            },
            Err(e) => (None, 0, Some(e.to_string())),
        },
        None => (None, 0, None),
    };

    Evidence {
        note,
        note_found,
        note_error,
        backing,
        cp_path,
        cp_count,
        cp_error,
    }
}

/// Information gathered about a single commit in the PR range.
#[derive(Debug)]
struct CommitInfo {
    sha: String,
    subject: String,
    note_found: bool,
    note_error: Option<String>,
    checkpoints_found: bool,
    lines_attributed: u64,
}

/// Result of placing head-diff added lines onto introducing commits in the range.
#[derive(Debug, Default)]
struct Placement {
    /// commit_sha -> (file_path -> set of original line numbers in that commit)
    per_commit_lines: HashMap<String, BTreeMap<String, BTreeSet<u32>>>,
    /// (commit, path, orig_line) -> final line number in head
    orig_to_final: HashMap<(String, String, u32), u32>,
    /// Final-line numbers that could not be attributed to any commit in the range.
    unattributed: Vec<u32>,
}

/// Place added lines of one file onto commits in `commit_set` using blame.
///
/// `blame = None` means the blame command itself failed: every line is unattributed.
/// A missing blame entry, or a blame commit outside the range, is also unattributed.
fn place_lines(
    path: &str,
    lines: &BTreeSet<u32>,
    blame: Option<&HashMap<u32, BlameLine>>,
    commit_set: &HashSet<String>,
    placement: &mut Placement,
) {
    let normed = path.replace('\\', "/");
    for &line in lines {
        let Some(blame_map) = blame else {
            placement.unattributed.push(line);
            continue;
        };
        match blame_map.get(&line) {
            Some(bl) if commit_set.contains(&bl.commit) => {
                placement
                    .per_commit_lines
                    .entry(bl.commit.clone())
                    .or_default()
                    .entry(normed.clone())
                    .or_default()
                    .insert(bl.orig_line);
                placement.orig_to_final.insert(
                    (bl.commit.clone(), normed.clone(), bl.orig_line),
                    line,
                );
            }
            _ => placement.unattributed.push(line),
        }
    }
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
    let git_dir = PathBuf::from(
        git(repo, &["rev-parse", "--absolute-git-dir"])?
            .trim(),
    );
    let evidence = load_evidence(repo, &git_dir, &sha, parent);
    let note = evidence.note;
    let note_found = evidence.note_found;
    let cp_path = evidence.cp_path;
    let cp_count = evidence.cp_count;
    let backing = evidence.backing;
    if let Some(err) = evidence.note_error {
        warnings.push(format!("Git AI note could not be read ({err}); added lines counted as unknown"));
    }
    if let Some(err) = evidence.cp_error {
        warnings.push(format!("raw checkpoints could not be read ({err}): AI claims could NOT be verified"));
    } else if note_found && cp_path.is_none() {
        warnings.push("raw Git AI checkpoints not found on this machine: AI claims in the note could NOT be verified".into());
    }
    if !note_found && warnings.is_empty() { warnings.push("no Git AI note on this commit: every added line is unknown".into()); }

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
            "schema_version": "prov.analysis/0.1",
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

    // List commits in base..head (oldest first). Same --parents output also detects merges.
    let rev_list = git(
        repo,
        &[
            "rev-list",
            "--reverse",
            "--parents",
            &format!("{base_sha}..{head_sha}"),
        ],
    )?;
    let mut commits: Vec<String> = Vec::new();
    for line in rev_list.lines().filter(|l| !l.is_empty()) {
        let mut parts = line.split_whitespace();
        let sha = parts
            .next()
            .ok_or_else(|| "empty rev-list line".to_string())?;
        let parent_count = parts.count();
        if parent_count > 1 {
            return Err(format!(
                "merge commit {} found in range: merge commits are not supported yet",
                &sha[..sha.len().min(10)]
            ));
        }
        commits.push(sha.to_string());
    }

    let commit_set: HashSet<String> = commits.iter().cloned().collect();

    // Step 1: Merge-base diff (three dots) -> added lines in head relative to merge base
    let diff = git(
        repo,
        &[
            "diff",
            "--unified=0",
            "--no-color",
            "--no-ext-diff",
            &format!("{base_sha}...{head_sha}"),
        ],
    )?;
    let added_in_head = parse_unified_diff_added_lines(&diff);

    let mapping = MappingPolicy::default();
    let exclusion = ExclusionPolicy::with_lockfiles();
    let mut total_counts = LineCounts::default();
    let mut total_known_human = 0u64;
    let mut total_conflicts = 0u64;
    let mut total_unbacked = 0u64;
    let mut unattributed_lines = 0u64;
    let mut warnings: Vec<String> = Vec::new();
    let mut commit_infos: Vec<CommitInfo> = Vec::new();
    let mut all_explanations: Vec<LineExplanation> = Vec::new();
    let mut unattributed_explanations: Vec<LineExplanation> = Vec::new();

    // Step 2: Place each added line onto its introducing commit (or count as unattributed /
    // excluded). Excluded paths are never blamed.
    let mut placement = Placement::default();

    for (file_path, line_numbers) in &added_in_head {
        let normed = file_path.replace('\\', "/");
        if exclusion.excludes(&normed) {
            total_counts.excluded += line_numbers.len() as u64;
            if explain {
                for &line in line_numbers {
                    unattributed_explanations.push(LineExplanation {
                        file: normed.clone(),
                        line,
                        commit: None,
                        category: Category::Excluded,
                        reason: "path matches exclusion policy".into(),
                        reason_code: ReasonCode::ExcludedByPolicy,
                    });
                }
            }
            continue;
        }

        let blame_output = git(
            repo,
            &[
                "blame",
                "--line-porcelain",
                &head_sha,
                "--",
                file_path,
            ],
        );
        let blame_map = match blame_output {
            Ok(out) => Some(parse_blame_porcelain(&out)),
            Err(e) => {
                warnings.push(format!(
                    "git blame failed for {normed}: {e}; {} line(s) counted as unknown",
                    line_numbers.len()
                ));
                None
            }
        };

        let before = placement.unattributed.len();
        place_lines(
            &normed,
            line_numbers,
            blame_map.as_ref(),
            &commit_set,
            &mut placement,
        );
        for &line in &placement.unattributed[before..] {
            unattributed_lines += 1;
            total_counts.unknown += 1;
            if explain {
                unattributed_explanations.push(LineExplanation {
                    file: normed.clone(),
                    line,
                    commit: None,
                    category: Category::Unknown,
                    reason: "could not attribute to a commit in the range -> unknown".into(),
                    reason_code: ReasonCode::Unattributed,
                });
            }
        }
    }

    if unattributed_lines > 0 {
        warnings.push(format!(
            "{unattributed_lines} added line(s) could not be attributed to a commit in the range: counted as unknown"
        ));
    }

    let Placement {
        mut per_commit_lines,
        orig_to_final,
        ..
    } = placement;

    // Step 3: For each commit, load its note + checkpoints and classify using ORIGINAL line numbers.
    let git_dir = PathBuf::from(
        git(repo, &["rev-parse", "--absolute-git-dir"])?
            .trim(),
    );

    for commit_sha in &commits {
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
        let evidence = load_evidence(repo, &git_dir, commit_sha, parent.as_deref());
        let note_found = evidence.note_found;
        let note = evidence.note;
        let checkpoints_found = evidence.cp_path.is_some();
        let backing = evidence.backing;
        if let Some(err) = &evidence.note_error {
            warnings.push(format!("commit {}: Git AI note could not be read ({err}); {} line(s) counted as unknown", &commit_sha[..10.min(commit_sha.len())], commit_line_count));
        }
        if let Some(err) = &evidence.cp_error {
            warnings.push(format!("commit {}: raw checkpoints could not be read ({err}): AI claims could NOT be verified", &commit_sha[..10.min(commit_sha.len())]));
        } else if note_found && !checkpoints_found && commit_line_count > 0 {
            warnings.push(format!("commit {}: raw checkpoints not found; AI claims could NOT be verified", &commit_sha[..10.min(commit_sha.len())]));
        }

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
        total_counts.merge(&r.metric.counts);
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
                if !note_found && e.reason_code == ReasonCode::NoNoteEntry {
                    e.reason_code = ReasonCode::NoNote;
                    e.reason = "no note -> unknown".to_string();
                }
            }
            all_explanations.append(&mut expl);
        }

        commit_infos.push(CommitInfo {
            sha: commit_sha.to_string(),
            subject,
            note_found,
            note_error: evidence.note_error,
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

    all_explanations.append(&mut unattributed_explanations);
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
                    "note_error": ci.note_error,
                    "checkpoints_found": ci.checkpoints_found,
                    "lines_in_final": ci.lines_attributed,
                })
            })
            .collect();

        let mut v = serde_json::json!({
            "schema_version": "prov.analysis/0.1",
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
            "unattributed_lines": unattributed_lines,
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
                        e.line,
                        e.category.as_str(),
                        commit_str,
                        e.reason
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

#[cfg(test)]
mod place_lines_tests {
    use super::*;

    fn bl(commit: &str, orig: u32, final_line: u32) -> BlameLine {
        BlameLine {
            commit: commit.to_string(),
            orig_line: orig,
            final_line,
        }
    }

    #[test]
    fn places_lines_blamed_to_commits_in_range() {
        let commit_set = HashSet::from(["aaa".to_string()]);
        let blame = HashMap::from([(2u32, bl("aaa", 1, 2)), (3u32, bl("aaa", 2, 3))]);
        let lines = BTreeSet::from([2u32, 3]);
        let mut placement = Placement::default();
        place_lines("f.txt", &lines, Some(&blame), &commit_set, &mut placement);
        assert!(placement.unattributed.is_empty());
        let placed = placement.per_commit_lines.get("aaa").unwrap();
        assert_eq!(placed.get("f.txt").unwrap(), &BTreeSet::from([1u32, 2]));
        assert_eq!(placement.orig_to_final.get(&("aaa".into(), "f.txt".into(), 1)), Some(&2));
        assert_eq!(placement.orig_to_final.get(&("aaa".into(), "f.txt".into(), 2)), Some(&3));
    }

    #[test]
    fn blamed_outside_range_is_unattributed() {
        let commit_set = HashSet::from(["aaa".to_string()]);
        let blame = HashMap::from([(1u32, bl("outside", 1, 1))]);
        let lines = BTreeSet::from([1u32]);
        let mut placement = Placement::default();
        place_lines("f.txt", &lines, Some(&blame), &commit_set, &mut placement);
        assert_eq!(placement.unattributed, vec![1]);
        assert!(placement.per_commit_lines.is_empty());
    }

    #[test]
    fn missing_from_blame_is_unattributed() {
        let commit_set = HashSet::from(["aaa".to_string()]);
        let blame = HashMap::from([(1u32, bl("aaa", 1, 1))]);
        let lines = BTreeSet::from([1u32, 99]);
        let mut placement = Placement::default();
        place_lines("f.txt", &lines, Some(&blame), &commit_set, &mut placement);
        assert_eq!(placement.unattributed, vec![99]);
        assert_eq!(
            placement
                .per_commit_lines
                .get("aaa")
                .unwrap()
                .get("f.txt")
                .unwrap(),
            &BTreeSet::from([1u32])
        );
    }

    #[test]
    fn blame_failed_none_marks_all_unattributed() {
        let commit_set = HashSet::from(["aaa".to_string()]);
        let lines = BTreeSet::from([1u32, 2, 3]);
        let mut placement = Placement::default();
        place_lines("f.txt", &lines, None, &commit_set, &mut placement);
        assert_eq!(placement.unattributed, vec![1, 2, 3]);
        assert!(placement.per_commit_lines.is_empty());
    }
}
