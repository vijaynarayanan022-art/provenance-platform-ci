//! Adapter: reads Git AI authorship notes (`refs/notes/ai`, schema `authorship/3.0.0`)
//! and maps them into `prov-core` categories.
//!
//! Git AI is used ONLY as a capture layer. This crate never trusts a note as final truth:
//! the mapping policy is explicit, and the lab findings (see HANDOFF.md) show Git AI can credit
//! unmarked human edits to an AI session. Human-marked lines map to `Unknown` + a human signal,
//! because the docs have no "human" category ("unknown is never human").

pub mod blame;
pub mod checkpoints;
pub mod classify;
pub mod diff;
pub mod note;

pub use blame::{parse_blame_porcelain, BlameLine};
pub use checkpoints::{parse_checkpoints_jsonl, Backing, RawCheckpoint};
pub use classify::{classify_added_lines, classify_added_lines_backed, classify_added_lines_explained, ExclusionPolicy, LineExplanation, MappingPolicy, PrAnalysis, ReasonCode};
pub use diff::{parse_unified_diff_added_lines, parse_unified_diff_added_lines_with_text};
pub use note::{parse_note, ActorKind, Entry, FileAttestation, LineRange, NoteMeta, ParseError, ParsedNote};
