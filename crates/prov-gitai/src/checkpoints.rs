//! Reader for Git AI's RAW checkpoint records (`.git/ai/working_logs/<base sha>/checkpoints.jsonl`,
//! archived as `old-<sha>` after the commit; debug builds never prune, release builds prune after 7 days).
//!
//! Why this exists: the final note can credit lines to an AI session that no checkpoint ever
//! covered (lab finding). A trace id that appears in the note but in NO raw checkpoint is an
//! "unbacked" claim and must not count as AI evidence.

use serde::{Deserialize, Deserializer};
use std::collections::HashSet;

/// Git AI writes explicit `null` for empty optional objects (e.g. `"agent_metadata":null` on
/// human checkpoints). `#[serde(default)]` only covers MISSING keys, so treat null as default too.
fn null_default<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

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
    pub blob_sha: Option<String>,
    #[serde(default, deserialize_with = "null_default")]
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
    #[serde(default, deserialize_with = "null_default")]
    pub entries: Vec<RawEntry>,
    /// Lines the AI actually added at this checkpoint (Git AI's own count).
    #[serde(default, deserialize_with = "null_default")]
    pub line_stats: RawLineStats,
    /// e.g. `edit_kind` ("bash"), `tool_use_id`, `transcript_path` (local path; privacy-sensitive).
    #[serde(default, deserialize_with = "null_default")]
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression: `prov analyze` crashed on a real repo with
    /// "invalid type: null, expected a map" because a human checkpoint carries
    /// `"agent_metadata":null` (and `"agent_id":null`).
    #[test]
    fn explicit_nulls_are_accepted() {
        let ai = r#"{"kind":"AiAgent","diff":"d","author":"a","entries":[{"file":"app.txt","blob_sha":"b","attributions":[],"line_attributions":[{"start_line":4,"end_line":7,"author_id":"s_1::t_1","overrode":null}]}],"timestamp":1,"agent_id":{"tool":"mock_ai","id":"x","model":"unknown"},"agent_metadata":null,"line_stats":{"additions":4,"deletions":0,"additions_sloc":4,"deletions_sloc":0},"api_version":"checkpoint/1.0.0","trace_id":"t_1"}"#;
        let human = r#"{"kind":"KnownHuman","diff":"d","author":"a","entries":null,"timestamp":2,"agent_id":null,"agent_metadata":null,"line_stats":null,"api_version":"checkpoint/1.0.0","known_human_metadata":{"editor":"vscode","editor_version":"1","extension_version":"2"},"trace_id":"t_2"}"#;
        let cps = parse_checkpoints_jsonl(&format!("{ai}\n{human}\n")).unwrap();
        assert_eq!(cps.len(), 2);
        assert!(cps[0].is_ai());
        assert!(!cps[1].is_ai());
        assert!(cps[1].agent_metadata.is_empty());
        let b = Backing::from_checkpoints(&cps);
        assert!(b.contains_trace("t_1"));
        assert!(!b.contains_trace("t_2"));
    }

    #[test]
    fn blank_lines_and_bom_are_tolerated() {
        assert_eq!(parse_checkpoints_jsonl("\u{feff}\n\n").unwrap().len(), 0);
    }
}
