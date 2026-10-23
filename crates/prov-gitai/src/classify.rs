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
    pub fn excludes(&self, path: &str) -> bool {
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

/// Machine-readable reason for a per-line classification (serialized snake_case).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasonCode {
    ExcludedByPolicy,
    AiBackedByCheckpoint,
    AiUnverified,
    UnbackedTrace,
    KnownHuman,
    Conflict,
    NoNoteEntry,
    NoNote,
    Unattributed,
}

/// Per-line explanation for the `--explain` output.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct LineExplanation {
    pub file: String,
    pub line: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    pub category: Category,
    pub reason: String,
    pub reason_code: ReasonCode,
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
    classify_added_lines_explained(note, added, mapping, exclusion, backing).0
}

/// Classify added lines AND produce per-line explanations.
/// Returns (analysis, explanations).
pub fn classify_added_lines_explained(
    note: &ParsedNote,
    added: &BTreeMap<String, BTreeSet<u32>>,
    mapping: &MappingPolicy,
    exclusion: &ExclusionPolicy,
    backing: Option<&Backing>,
) -> (PrAnalysis, Vec<LineExplanation>) {
    let mut unbacked = 0u64;
    let mut counts = LineCounts::default();
    let mut known_human = 0u64;
    let mut conflicts = 0u64;
    let mut explanations = Vec::new();

    for (path, lines) in added {
        let normed = norm(path);
        if exclusion.excludes(&normed) {
            counts.add(Category::Excluded, lines.len() as u64);
            for &line in lines {
                explanations.push(LineExplanation {
                    file: normed.clone(),
                    line,
                    commit: None,
                    category: Category::Excluded,
                    reason: "path matches exclusion policy".into(),
                    reason_code: ReasonCode::ExcludedByPolicy,
                });
            }
            continue;
        }
        let file = note.files.iter().find(|f| norm(&f.path) == normed);
        for &line in lines {
            let mut ai = false;
            let mut human = false;
            let mut unbacked_here = false;
            let mut reason_parts: Vec<String> = Vec::new();
            if let Some(f) = file {
                for e in &f.entries {
                    if e.ranges.iter().any(|r| r.contains(line)) {
                        match e.kind {
                            ActorKind::AiSession | ActorKind::Prompt => {
                                let trace_desc = e.trace.as_deref().unwrap_or("none");
                                let backed = match (backing, e.trace.as_deref()) {
                                    (Some(b), Some(t)) => b.contains_trace(t),
                                    _ => true,
                                };
                                if backed {
                                    ai = true;
                                    if backing.is_some() {
                                        reason_parts.push(format!(
                                            "AI session claim, trace {} backed by checkpoint",
                                            trace_desc
                                        ));
                                    } else {
                                        reason_parts.push(format!(
                                            "AI session claim, trace {} (no checkpoints to verify)",
                                            trace_desc
                                        ));
                                    }
                                } else {
                                    unbacked_here = true;
                                    reason_parts.push(format!(
                                        "trace {} not in raw checkpoints -> unknown",
                                        trace_desc
                                    ));
                                }
                            }
                            ActorKind::KnownHuman => {
                                human = true;
                                reason_parts.push("known-human marker -> unknown (no human category)".into());
                            }
                        }
                    }
                }
            }
            if reason_parts.is_empty() {
                reason_parts.push("no note entry -> unknown".into());
            }
            if unbacked_here && !ai && !human {
                unbacked += 1;
            }

            let (cat, reason_code) = match (ai, human) {
                (true, false) => {
                    let rc = if backing.is_some() {
                        ReasonCode::AiBackedByCheckpoint
                    } else {
                        ReasonCode::AiUnverified
                    };
                    (mapping.ai_session, rc)
                }
                (false, true) => {
                    known_human += 1;
                    (Category::Unknown, ReasonCode::KnownHuman)
                }
                (true, true) => {
                    conflicts += 1;
                    reason_parts.push("conflicting AI + human claims -> unknown".into());
                    (Category::Unknown, ReasonCode::Conflict)
                }
                (false, false) => {
                    let rc = if unbacked_here {
                        ReasonCode::UnbackedTrace
                    } else {
                        ReasonCode::NoNoteEntry
                    };
                    (Category::Unknown, rc)
                }
            };
            counts.add(cat, 1);

            explanations.push(LineExplanation {
                file: normed.clone(),
                line,
                commit: None,
                category: cat,
                reason: reason_parts.join("; "),
                reason_code,
            });
        }
    }

    let analysis = PrAnalysis {
        metric: PrMetric::from_counts(counts),
        known_human_lines: known_human,
        conflicting_lines: conflicts,
        unbacked_ai_lines: unbacked,
    };

    (analysis, explanations)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NoteMeta;

    #[test]
    fn backed_matches_explained() {
        let note = ParsedNote {
            files: vec![],
            meta: NoteMeta::default(),
        };
        let mut added = BTreeMap::new();
        added.insert("a.txt".to_string(), BTreeSet::from([1, 2]));
        let mapping = MappingPolicy::default();
        let exclusion = ExclusionPolicy::with_lockfiles();

        let backed_res = classify_added_lines_backed(&note, &added, &mapping, &exclusion, None);
        let (explained_res, expls) =
            classify_added_lines_explained(&note, &added, &mapping, &exclusion, None);

        assert_eq!(backed_res, explained_res);
        assert_eq!(expls.len(), 2);
        assert_eq!(expls[0].category, Category::Unknown);
        assert_eq!(expls[0].reason_code, ReasonCode::NoNoteEntry);

        let json = serde_json::to_value(&expls[0]).unwrap();
        assert_eq!(json["category"], "unknown");
        assert_eq!(json["reason_code"], "no_note_entry");
    }
}
