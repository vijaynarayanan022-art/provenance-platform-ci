//! Characterisation tests built from REAL Git AI output captured in the lab
//! (mock_ai test preset, Git AI development:1.7.6, Windows).
//! Scenario: base commit has 3 human lines; the commit under test adds lines 4-9
//! (lines 4-7 written by the fake AI, lines 8-9 typed by a human after the AI checkpoint).

use prov_core::PrMetric;
use prov_gitai::{classify_added_lines, parse_note, ExclusionPolicy, MappingPolicy};
use std::collections::{BTreeMap, BTreeSet};

fn added_4_to_9() -> BTreeMap<String, BTreeSet<u32>> {
    let mut m = BTreeMap::new();
    m.insert("app.txt".to_string(), (4..=9).collect::<BTreeSet<u32>>());
    m
}

fn approx(a: Option<f64>, b: f64) {
    let a = a.unwrap();
    assert!((a - b).abs() < 1e-9, "{a} != {b}");
}

#[test]
fn test2_human_marked_lines_are_not_ai() {
    let note = parse_note(include_str!("fixtures/test2-note.txt")).unwrap();
    let r = classify_added_lines(&note, &added_4_to_9(), &MappingPolicy::default(), &ExclusionPolicy::default());
    assert_eq!(r.metric.counts.probable, 4);
    assert_eq!(r.metric.counts.unknown, 2);
    assert_eq!(r.known_human_lines, 2);
    assert_eq!(r.metric.eligible, 6);
    approx(r.metric.ai_deterministic_pct, 0.0);
}

/// KNOWN LIMITATION of Git AI (observed in lab): human lines typed after the last AI checkpoint
/// with no human marker were credited to the AI session. We pin the behaviour so a future Git AI
/// upgrade that fixes (or changes) it shows up as a test failure, and so the engine knows the
/// note alone cannot be trusted as proof.
#[test]
fn test1_unmarked_human_lines_get_credited_to_ai_by_git_ai() {
    let note = parse_note(include_str!("fixtures/test1-note.txt")).unwrap();
    let r = classify_added_lines(&note, &added_4_to_9(), &MappingPolicy::default(), &ExclusionPolicy::default());
    assert_eq!(r.metric.counts.probable, 6, "the AI claim has no raw checkpoint verification");
    assert_eq!(r.metric.counts.unknown, 0);
    approx(r.metric.ai_deterministic_pct, 0.0);
}

#[test]
fn note_metadata_is_exposed() {
    let note = parse_note(include_str!("fixtures/test2-note.txt")).unwrap();
    assert_eq!(note.meta.schema_version, "authorship/3.0.0");
    assert_eq!(note.meta.sessions.values().next().unwrap().agent_id.tool, "mock_ai");
    assert_eq!(note.meta.humans.len(), 1);
}

#[test]
fn lines_with_no_attestation_are_unknown_never_human() {
    let note = parse_note(include_str!("fixtures/test2-note.txt")).unwrap();
    let mut added = BTreeMap::new();
    added.insert("other.txt".to_string(), (1..=3).collect::<BTreeSet<u32>>());
    let r = classify_added_lines(&note, &added, &MappingPolicy::default(), &ExclusionPolicy::default());
    assert_eq!(r.metric.counts.unknown, 3);
    assert_eq!(r.known_human_lines, 0);
}

#[test]
fn lockfile_lines_are_excluded_from_the_denominator() {
    let note = parse_note(include_str!("fixtures/test2-note.txt")).unwrap();
    let mut added = added_4_to_9();
    added.insert("Cargo.lock".to_string(), (1..=500).collect::<BTreeSet<u32>>());
    let r = classify_added_lines(&note, &added, &MappingPolicy::default(), &ExclusionPolicy::with_lockfiles());
    assert_eq!(r.metric.counts.excluded, 500);
    assert_eq!(r.metric.eligible, 6);
    let _: PrMetric = r.metric;
}

#[test]
fn windows_style_paths_match() {
    let note = parse_note(include_str!("fixtures/test2-note.txt")).unwrap();
    let mut added = BTreeMap::new();
    added.insert("app.txt".replace('/', "\\"), (4..=7).collect::<BTreeSet<u32>>());
    let r = classify_added_lines(&note, &added, &MappingPolicy::default(), &ExclusionPolicy::default());
    assert_eq!(r.metric.counts.probable, 4);
}

/// REAL tool (Codex CLI via Git AI hooks, Windows). Codex appended 4 lines (4-7); the human then
/// typed 2 lines (8-9) in Notepad (no Git AI editor extension). Git AI credited ALL 6 lines to
/// Codex, under two different trace ids for the same session. Pinned as a known limitation.
#[test]
fn real_codex_unmarked_human_lines_credited_to_ai() {
    let note = parse_note(include_str!("fixtures/real1-codex-note.txt")).unwrap();
    let mut added = BTreeMap::new();
    added.insert("notes.txt".to_string(), (4..=9).collect::<BTreeSet<u32>>());
    let r = classify_added_lines(&note, &added, &MappingPolicy::default(), &ExclusionPolicy::default());
    assert_eq!(r.metric.counts.probable, 6);
    assert_eq!(r.known_human_lines, 0);
    let traces: Vec<_> = note.files[0].entries.iter().filter_map(|e| e.trace.clone()).collect();
    assert_eq!(traces.len(), 2, "same session, two trace ids; the 2nd covers the human-typed lines");
    assert_eq!(note.meta.sessions.values().next().unwrap().agent_id.tool, "codex");
}

use prov_gitai::{classify_added_lines_backed, parse_checkpoints_jsonl, Backing};

/// Raw-checkpoint reconciliation on the real Codex run. The checkpoint file was the ONLY record
/// (kind AiAgent, trace t_f608..., lines 3-7). Trace t_31f7... (lines 8-9, human-typed) appears in
/// the note but in no checkpoint -> unbacked -> Unknown. The fixture is the REAL raw file.
#[test]
fn real_codex_unbacked_trace_becomes_unknown() {
    let note = parse_note(include_str!("fixtures/real1-codex-note.txt")).unwrap();
    let cps = parse_checkpoints_jsonl(include_str!("fixtures/real1-checkpoints.jsonl")).unwrap();
    let backing = Backing::from_checkpoints(&cps);
    assert_eq!(backing.len(), 1);
    let mut added = BTreeMap::new();
    added.insert("notes.txt".to_string(), (4..=9).collect::<BTreeSet<u32>>());
    let r = classify_added_lines_backed(&note, &added, &MappingPolicy::default(), &ExclusionPolicy::default(), Some(&backing));
    assert_eq!(r.metric.counts.strong, 4);
    assert_eq!(r.metric.counts.unknown, 2);
    assert_eq!(r.unbacked_ai_lines, 2);
    approx(r.metric.ai_deterministic_pct, 400.0 / 6.0);
}

#[test]
fn without_raw_checkpoints_ai_claims_become_probable() {
    let note = parse_note(include_str!("fixtures/real1-codex-note.txt")).unwrap();
    let mut added = BTreeMap::new();
    added.insert("notes.txt".to_string(), (4..=9).collect::<BTreeSet<u32>>());
    let r = classify_added_lines_backed(&note, &added, &MappingPolicy::default(), &ExclusionPolicy::default(), None);
    assert_eq!(r.metric.counts.probable, 6);
    assert_eq!(r.unbacked_ai_lines, 0);
}

/// Second, independent signal in the same raw record: Git AI itself counted only 4 AI additions at
/// checkpoint time, but the final note/stats claim 6. The 2-line gap is the over-credit.
#[test]
fn real_codex_checkpoint_line_stats_expose_the_gap() {
    let cps = parse_checkpoints_jsonl(include_str!("fixtures/real1-checkpoints.jsonl")).unwrap();
    assert_eq!(cps.len(), 1);
    let ai_added: u32 = cps.iter().filter(|c| c.is_ai()).map(|c| c.line_stats.additions).sum();
    assert_eq!(ai_added, 4);
    assert_eq!(cps[0].agent_metadata.get("edit_kind").map(String::as_str), Some("bash"));
    let claimed_by_note = 6u32;
    assert_eq!(claimed_by_note - ai_added, 2);
}
