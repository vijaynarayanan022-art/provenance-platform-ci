use crate::checkpoints::Backing;
use crate::note::{ActorKind, ParsedNote};
use prov_core::{Category, LineCounts, PrMetric};
use std::collections::{BTreeMap, BTreeSet};

/// How Git AI evidence maps to our categories. Explicit so it can be changed by a named
/// profile later (doc 04: thresholds/profiles are versioned, never silently edited).
#[derive(Debug, Clone)]
pub struct MappingPolicy {
    /// Category given to lines Git AI credits to an AI session.
    /// Default `AiStrong`: Git AI sees a post-edit checkpoint but our lab showed it can
    /// over-credit, so we do NOT default to `AiDirect` (needs accepted edit + verified survival).
    pub ai_session: Category,
}

impl Default for MappingPolicy {
    fn default() -> Self {
        MappingPolicy { ai_session: Category::AiStrong }
    }
}

/// Paths excluded from the eligible denominator (e.g. lockfiles, generated code).
#[derive(Debug, Clone, Default)]
pub struct ExclusionPolicy {
    pub path_suffixes: Vec<String>,
}

impl ExclusionPolicy {
    pub fn with_lockfiles() -> Self {
        ExclusionPolicy {
            path_suffixes: ["Cargo.lock", "package-lock.json", "yarn.lock", "pnpm-lock.yaml"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        }
    }
    fn excludes(&self, path: &str) -> bool {
        self.path_suffixes.iter().any(|s| path.ends_with(s.as_str()))
    }
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct PrAnalysis {
    pub metric: PrMetric,
    /// Lines that carry a known-human signal. They are counted as `Unknown` in the metric
    /// (no "human" category exists) but reported separately so coverage can show them.
    pub known_human_lines: u64,
    /// Lines where the note contradicted itself (AI and human both claim it) -> `Unknown`.
    pub conflicting_lines: u64,
    /// AI-credited lines whose trace id has NO raw checkpoint behind it (only counted when a
    /// `Backing` was supplied). They are classified `Unknown`, never AI.
    pub unbacked_ai_lines: u64,
}

fn norm(p: &str) -> String {
    p.replace('\\', "/")
}

/// Classify the *added lines* of one commit (path -> set of 1-based line numbers in the
/// commit's version of the file) using that commit's Git AI note.
///
/// Multi-commit PRs: call once per commit with that commit's added lines and merge counts
/// (TODO: lines later rewritten inside the same PR; needs the per-commit lineage work).
pub fn classify_added_lines(
    note: &ParsedNote,
    added: &BTreeMap<String, BTreeSet<u32>>,
    mapping: &MappingPolicy,
    exclusion: &ExclusionPolicy,
) -> PrAnalysis {
    classify_added_lines_backed(note, added, mapping, exclusion, None)
}

/// Same as `classify_added_lines`, but when `backing` (trace ids found in the raw checkpoints) is
/// given, an AI-session claim whose trace id is not backed by any raw AI checkpoint becomes `Unknown`.
///
/// Limits (be honest in reports): this is trace-level, not line-level. It catches lines credited
/// under a trace no checkpoint ever recorded (the lab over-credit). It cannot catch human lines
/// that fall inside a range whose trace IS backed.
pub fn classify_added_lines_backed(
    note: &ParsedNote,
    added: &BTreeMap<String, BTreeSet<u32>>,
    mapping: &MappingPolicy,
    exclusion: &ExclusionPolicy,
    backing: Option<&Backing>,
) -> PrAnalysis {
    let mut unbacked = 0u64;
    let mut counts = LineCounts::default();
    let mut known_human = 0u64;
    let mut conflicts = 0u64;

    for (path, lines) in added {
        let path = norm(path);
        if exclusion.excludes(&path) {
            counts.add(Category::Excluded, lines.len() as u64);
            continue;
        }
        let file = note.files.iter().find(|f| norm(&f.path) == path);
        for &line in lines {
            let mut ai = false;
            let mut human = false;
            let mut unbacked_here = false;
            if let Some(f) = file {
                for e in &f.entries {
                    if e.ranges.iter().any(|r| r.contains(line)) {
                        match e.kind {
                            ActorKind::AiSession | ActorKind::Prompt => {
                                let backed = match (backing, e.trace.as_deref()) {
                                    (Some(b), Some(t)) => b.contains_trace(t),
                                    _ => true,
                                };
                                if backed {
                                    ai = true;
                                } else {
                                    unbacked_here = true;
                                }
                            }
                            ActorKind::KnownHuman => human = true,
                        }
                    }
                }
            }
            if unbacked_here && !ai && !human {
                unbacked += 1;
            }
            match (ai, human) {
                (true, false) => counts.add(mapping.ai_session, 1),
                (false, true) => {
                    known_human += 1;
                    counts.add(Category::Unknown, 1);
                }
                (true, true) => {
                    conflicts += 1;
                    counts.add(Category::Unknown, 1);
                }
                (false, false) => counts.add(Category::Unknown, 1),
            }
        }
    }
    PrAnalysis {
        metric: PrMetric::from_counts(counts),
        known_human_lines: known_human,
        conflicting_lines: conflicts,
        unbacked_ai_lines: unbacked,
    }
}
