use serde::Deserialize;
use std::collections::HashMap;
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineRange {
    pub start: u32,
    pub end: u32, // inclusive
}

impl LineRange {
    pub fn contains(&self, line: u32) -> bool {
        line >= self.start && line <= self.end
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActorKind {
    /// `s_...` key: an AI agent session.
    AiSession,
    /// `h_...` key: a known-human edit (editor extension saw a human save).
    KnownHuman,
    /// Any other key: legacy prompt-hash style.
    Prompt,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Key without the trace suffix, e.g. `s_4b9a24e746de22`.
    pub key: String,
    /// Trace id after `::`, e.g. `t_3902a067f29e63`.
    pub trace: Option<String>,
    pub kind: ActorKind,
    pub ranges: Vec<LineRange>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileAttestation {
    pub path: String,
    pub entries: Vec<Entry>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct AgentId {
    #[serde(default)]
    pub tool: String,
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub model: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct SessionMeta {
    #[serde(default)]
    pub agent_id: AgentId,
    #[serde(default)]
    pub human_author: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct HumanMeta {
    #[serde(default)]
    pub author: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct NoteMeta {
    #[serde(default)]
    pub schema_version: String,
    #[serde(default)]
    pub git_ai_version: String,
    #[serde(default)]
    pub base_commit_sha: String,
    #[serde(default)]
    pub sessions: HashMap<String, SessionMeta>,
    #[serde(default)]
    pub humans: HashMap<String, HumanMeta>,
    #[serde(default)]
    pub prompts: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Clone)]
pub struct ParsedNote {
    pub files: Vec<FileAttestation>,
    pub meta: NoteMeta,
}

#[derive(Debug)]
pub enum ParseError {
    MissingSeparator,
    BadJson(String),
    BadRange(String),
    EntryBeforeFile(String),
    UnsupportedSchema(String),
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseError::MissingSeparator => write!(f, "note has no `---` separator"),
            ParseError::BadJson(e) => write!(f, "metadata JSON invalid: {e}"),
            ParseError::BadRange(r) => write!(f, "bad line range: {r:?}"),
            ParseError::EntryBeforeFile(l) => write!(f, "attestation line before any file path: {l:?}"),
            ParseError::UnsupportedSchema(s) => write!(f, "unsupported schema_version {s:?}"),
        }
    }
}
impl std::error::Error for ParseError {}

fn parse_ranges(s: &str) -> Result<Vec<LineRange>, ParseError> {
    let mut out = Vec::new();
    for part in s.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let bad = || ParseError::BadRange(part.to_string());
        let (a, b) = match part.split_once('-') {
            Some((a, b)) => (a.parse::<u32>().map_err(|_| bad())?, b.parse::<u32>().map_err(|_| bad())?),
            None => {
                let n = part.parse::<u32>().map_err(|_| bad())?;
                (n, n)
            }
        };
        if a == 0 || b < a {
            return Err(bad());
        }
        out.push(LineRange { start: a, end: b });
    }
    Ok(out)
}

/// Parse the text of `git notes --ref=ai show <commit>`.
///
/// Shape (authorship/3.0.0):
/// ```text
/// path/to/file
///   s_<session>::t_<trace> 4-7,10
///   h_<human> 8-9
/// ---
/// { ...json metadata... }
/// ```
pub fn parse_note(text: &str) -> Result<ParsedNote, ParseError> {
    let text = text.replace("\r\n", "\n");
    let mut head = String::new();
    let mut json = String::new();
    let mut seen_sep = false;
    for line in text.lines() {
        if !seen_sep && line.trim_end() == "---" {
            seen_sep = true;
            continue;
        }
        let target = if seen_sep { &mut json } else { &mut head };
        target.push_str(line);
        target.push('\n');
    }
    if !seen_sep {
        return Err(ParseError::MissingSeparator);
    }

    let meta: NoteMeta = serde_json::from_str(&json).map_err(|e| ParseError::BadJson(e.to_string()))?;
    if !meta.schema_version.starts_with("authorship/3.") {
        return Err(ParseError::UnsupportedSchema(meta.schema_version));
    }

    let mut files: Vec<FileAttestation> = Vec::new();
    for line in head.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let indented = line.starts_with(' ') || line.starts_with('\t');
        if !indented {
            files.push(FileAttestation { path: line.trim().to_string(), entries: Vec::new() });
            continue;
        }
        let cur = files
            .last_mut()
            .ok_or_else(|| ParseError::EntryBeforeFile(line.to_string()))?;
        let body = line.trim();
        let (id, ranges) = body.split_once(' ').ok_or_else(|| ParseError::BadRange(body.to_string()))?;
        let (key, trace) = match id.split_once("::") {
            Some((k, t)) => (k.to_string(), Some(t.to_string())),
            None => (id.to_string(), None),
        };
        let kind = if key.starts_with("s_") {
            ActorKind::AiSession
        } else if key.starts_with("h_") {
            ActorKind::KnownHuman
        } else {
            ActorKind::Prompt
        };
        cur.entries.push(Entry { key, trace, kind, ranges: parse_ranges(ranges)? });
    }
    Ok(ParsedNote { files, meta })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_note_without_separator() {
        assert!(matches!(parse_note("app.txt\n  s_1 1-2\n"), Err(ParseError::MissingSeparator)));
    }

    #[test]
    fn parses_single_lines_and_ranges() {
        let n = "a.rs\n  s_1::t_1 1,3-4\n---\n{\"schema_version\":\"authorship/3.0.0\"}";
        let p = parse_note(n).unwrap();
        let r = &p.files[0].entries[0].ranges;
        assert_eq!(r, &vec![LineRange { start: 1, end: 1 }, LineRange { start: 3, end: 4 }]);
    }

    #[test]
    fn rejects_other_schema() {
        let n = "a.rs\n  s_1 1\n---\n{\"schema_version\":\"authorship/9.0.0\"}";
        assert!(matches!(parse_note(n), Err(ParseError::UnsupportedSchema(_))));
    }
}
