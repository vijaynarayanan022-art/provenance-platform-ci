//! Reader for Git AI's RAW checkpoint records (`.git/ai/working_logs/<base sha>/checkpoints.jsonl`,
//! archived as `old-<sha>` after the commit; debug builds never prune, release builds prune after 7 days).
//!
//! Why this exists: the final note can credit lines to an AI session that no checkpoint ever
//! covered (lab finding). A trace id that appears in the note but in NO raw checkpoint is an
//! "unbacked" claim and must not count as AI evidence.

use serde::Deserialize;
use std::collections::HashSet;

#[derive(Debug, Clone, Deserialize, Default)]
pub struct RawAgentId {
    #[serde(default)]
    pub tool: String,
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub model: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct RawLineAttribution {
    pub start_line: u32,
    pub end_line: u32,
    #[serde(default)]
    pub author_id: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct RawEntry {
    #[serde(default)]
    pub file: String,
    #[serde(default)]
    pub line_attributions: Vec<RawLineAttribution>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct RawLineStats {
    #[serde(default)]
    pub additions: u32,
    #[serde(default)]
    pub deletions: u32,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct RawCheckpoint {
    /// `Human`, `AiAgent`, `AiTab`, `KnownHuman`.
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub trace_id: Option<String>,
    #[serde(default)]
    pub agent_id: Option<RawAgentId>,
    #[serde(default)]
    pub timestamp: u64,
    #[serde(default)]
    pub entries: Vec<RawEntry>,
    /// Lines the AI actually added at this checkpoint (Git AI's own count).
    #[serde(default)]
    pub line_stats: RawLineStats,
    /// e.g. `edit_kind` ("bash"), `tool_use_id`, `transcript_path` (local path; privacy-sensitive).
    #[serde(default)]
    pub agent_metadata: std::collections::HashMap<String, String>,
}

impl RawCheckpoint {
    pub fn is_ai(&self) -> bool {
        self.kind == "AiAgent" || self.kind == "AiTab"
    }
}

/// Parse a `checkpoints.jsonl` file (one JSON object per line). Blank lines are skipped.
pub fn parse_checkpoints_jsonl(text: &str) -> Result<Vec<RawCheckpoint>, String> {
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.trim().trim_start_matches('\u{feff}');
        if line.is_empty() {
            continue;
        }
        let c: RawCheckpoint =
            serde_json::from_str(line).map_err(|e| format!("checkpoint line {}: {e}", i + 1))?;
        out.push(c);
    }
    Ok(out)
}

/// The set of trace ids that are backed by at least one raw AI checkpoint.
#[derive(Debug, Clone, Default)]
pub struct Backing {
    traces: HashSet<String>,
}

impl Backing {
    pub fn from_checkpoints(cps: &[RawCheckpoint]) -> Self {
        let mut traces = HashSet::new();
        for c in cps.iter().filter(|c| c.is_ai()) {
            if let Some(t) = &c.trace_id {
                traces.insert(t.clone());
            }
            for e in &c.entries {
                for la in &e.line_attributions {
                    if let Some((_, t)) = la.author_id.split_once("::") {
                        traces.insert(t.to_string());
                    }
                }
            }
        }
        Backing { traces }
    }
    pub fn contains_trace(&self, t: &str) -> bool {
        self.traces.contains(t)
    }
    pub fn len(&self) -> usize {
        self.traces.len()
    }
    pub fn is_empty(&self) -> bool {
        self.traces.is_empty()
    }
}
