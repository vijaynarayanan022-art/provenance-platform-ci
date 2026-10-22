//! `prov analyze` - evidence-based AI attribution report for ONE commit.
//!
//! Reads: `git diff-tree` (added lines), the Git AI note on the commit, and Git AI's raw
//! checkpoints archived on this machine. Writes nothing; every git call is read-only.

use prov_gitai::{
    classify_added_lines_backed, parse_checkpoints_jsonl, parse_note, parse_unified_diff_added_lines,
    Backing, ExclusionPolicy, MappingPolicy, NoteMeta, ParsedNote,
};
use std::path::{Path, PathBuf};
use std::process::Command;

const USAGE: &str = "usage: prov analyze [--repo PATH] [--commit REV] [--json]\n\n  --repo    repository path (default: current directory)\n  --commit  commit to analyze (default: HEAD)\n  --json    machine-readable output\n";

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
        let p = git_dir.join("ai").join("working_logs").join(dir).join("checkpoints.jsonl");
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

fn run(args: &[String]) -> Result<(), String> {
    if args.first().map(String::as_str) != Some("analyze") {
        return Err(format!("usage\n{USAGE}"));
    }
    let (mut repo, mut commit, mut json) = (PathBuf::from("."), "HEAD".to_string(), false);
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--repo" => { i += 1; repo = PathBuf::from(args.get(i).ok_or("usage\n--repo needs a value")?); }
            "--commit" => { i += 1; commit = args.get(i).ok_or("usage\n--commit needs a value")?.clone(); }
            "--json" => json = true,
            other => return Err(format!("usage\nunknown argument {other:?}\n{USAGE}")),
        }
        i += 1;
    }

    let sha = git(&repo, &["rev-parse", "--verify", &format!("{commit}^{{commit}}")])?.trim().to_string();
    let subject = git(&repo, &["log", "-1", "--format=%s", &sha])?.trim().to_string();
    let parents_line = git(&repo, &["rev-list", "--parents", "-n", "1", &sha])?;
    let parents: Vec<&str> = parents_line.split_whitespace().skip(1).collect();
    if parents.len() > 1 {
        return Err("merge commits are not supported yet".to_string());
    }
    let parent = parents.first().copied();

    let diff = git(&repo, &["diff-tree", "-p", "-r", "--root", "--unified=0", "--no-color", "--no-ext-diff", &sha])?;
    let added = parse_unified_diff_added_lines(&diff);

    let mut warnings: Vec<String> = Vec::new();
    let note_text = git(&repo, &["notes", "--ref=ai", "show", &sha]).ok();
    let note_found = note_text.is_some();
    let note = match note_text {
        Some(t) => parse_note(&t).map_err(|e| format!("cannot read Git AI note: {e}"))?,
        None => {
            warnings.push("no Git AI note on this commit: every added line is unknown".into());
            ParsedNote { files: vec![], meta: NoteMeta::default() }
        }
    };

    let git_dir = PathBuf::from(git(&repo, &["rev-parse", "--absolute-git-dir"])?.trim());
    let cp_path = find_checkpoints(&git_dir, parent);
    let (backing, cp_count) = match &cp_path {
        Some(p) => {
            let text = std::fs::read_to_string(p).map_err(|e| format!("cannot read {}: {e}", p.display()))?;
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

    let r = classify_added_lines_backed(&note, &added, &MappingPolicy::default(), &ExclusionPolicy::with_lockfiles(), backing.as_ref());
    let m = &r.metric;
    if r.unbacked_ai_lines > 0 {
        warnings.push(format!("{} line(s) credited to AI by the note have no raw checkpoint behind them: counted as unknown", r.unbacked_ai_lines));
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
        println!("Commit {} - {}", &sha[..sha.len().min(10)], subject);
        println!("Eligible added lines: {}   (excluded: {})", m.eligible, c.excluded);
        println!("  ai_direct {}  ai_strong {}  ai_probable {}  mixed {}  unknown {}", c.direct, c.strong, c.probable, c.mixed, c.unknown);
        println!("AI deterministic: {}    AI linked: {}", pct(m.ai_deterministic_pct), pct(m.ai_linked_pct));
        println!("Human-signal lines (counted as unknown): {}   Conflicts: {}   Unbacked AI claims: {}", r.known_human_lines, r.conflicting_lines, r.unbacked_ai_lines);
        match &cp_path {
            Some(p) => println!("Raw checkpoints: {} record(s) from {}", cp_count, p.display()),
            None => println!("Raw checkpoints: not found"),
        }
        for w in &warnings {
            println!("WARNING: {w}");
        }
    }
    Ok(())
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
