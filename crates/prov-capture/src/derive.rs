use similar::{capture_diff_slices, Algorithm, DiffTag};
use std::collections::{HashMap, HashSet};

pub const MIN_MIXED_OVERLAP: f64 = 0.5;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StepKind {
    Ai { degraded: bool },
    External,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Step {
    pub kind: StepKind,
    pub after: Vec<u8>,
    pub attribution: Option<StepAttribution>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StepAttribution {
    pub adapter_id: Option<String>,
    pub tool_id: Option<String>,
    pub run_id: Option<String>,
    pub session_id: Option<String>,
    pub operation: Option<String>,
    pub primary_basis: Option<String>,
    pub score_milli: u16,
    pub penalties_milli: Vec<u16>,
    pub supporting_basis: Vec<String>,
    pub limiting_basis: Vec<String>,
    pub direct_lines: Vec<Vec<u8>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Evidence {
    pub parent: Vec<u8>,
    pub steps: Vec<Step>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Label {
    AiDirect,
    AiStrong,
    AiProbable,
    Mixed,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LineLabel {
    pub line: usize,
    pub label: Label,
    pub added: bool,
    pub blank: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    pub d: usize,
    pub s: usize,
    pub p: usize,
    pub m: usize,
    pub u: usize,
    pub e: usize,
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
enum Owner {
    Base,
    External,
    Gap,
    Ai,
    Direct,
    AiWeak,
    AiEdited,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct OwnedLine {
    owner: Owner,
    candidates: u8,
    source_step: Option<usize>,
    ambiguous: bool,
}

#[derive(Clone, Copy)]
struct DuplicateOwners {
    candidates: u8,
    ambiguous: bool,
}

const fn owner_candidate(owner: Owner) -> u8 {
    1 << owner as u8
}

const AI_ANCESTRY_CANDIDATES: u8 = owner_candidate(Owner::Ai)
    | owner_candidate(Owner::Direct)
    | owner_candidate(Owner::AiWeak)
    | owner_candidate(Owner::AiEdited);

fn all_candidates_have_ai_ancestry(candidates: u8) -> bool {
    candidates != 0 && candidates & !AI_ANCESTRY_CANDIDATES == 0
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttributedLine {
    pub line: LineLabel,
    pub source_step: Option<usize>,
    pub ambiguous: bool,
    pub unobserved_edit: Option<UnobservedEditKind>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnobservedEditKind {
    ExactSurvivorInChangedFile,
    MixedByTokenOverlap,
    MixedBySimilarity,
    UnmatchedUnknown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnobservedAiLine {
    pub text: Vec<u8>,
    pub source_step: Option<usize>,
    pub label: Label,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnobservedReplacementHunk {
    pub removed_ai: Vec<UnobservedAiLine>,
    pub added_line_indices: Vec<usize>,
}

pub fn derive_lines(ev: &Evidence, committed: &[u8]) -> Vec<LineLabel> {
    derive_lines_with_attribution(ev, committed)
        .into_iter()
        .map(|line| line.line)
        .collect()
}

pub fn derive_lines_with_attribution(ev: &Evidence, committed: &[u8]) -> Vec<AttributedLine> {
    let parent = split_lines(&ev.parent);
    let mut previous = parent.clone();
    let mut owners = vec![
        OwnedLine {
            owner: Owner::Base,
            candidates: owner_candidate(Owner::Base),
            source_step: None,
            ambiguous: false,
        };
        previous.len()
    ];

    for (step_index, step) in ev.steps.iter().enumerate() {
        let next = split_lines(&step.after);
        let direct_lines = step
            .attribution
            .as_ref()
            .map(|attribution| attribution.direct_lines.as_slice())
            .unwrap_or_default();
        owners = replay(
            &previous,
            &owners,
            &next,
            step.kind,
            step_index,
            direct_lines,
        );
        previous = next;
    }

    let committed_lines = split_lines(committed);
    let committed_owners = replay(
        &previous,
        &owners,
        &committed_lines,
        StepKind::Unknown,
        ev.steps.len(),
        &[],
    );
    let added = added_lines(&parent, &committed_lines);
    let duplicate_owners = duplicate_owners(&committed_owners);

    committed_lines
        .iter()
        .enumerate()
        .map(|(index, line)| {
            let owned = committed_owners[index];
            let duplicates = duplicate_owners[index];
            let candidates = owned.candidates | duplicates.candidates;
            let ambiguous = owned.ambiguous || duplicates.ambiguous;
            let label = if !added[index] || !all_candidates_have_ai_ancestry(candidates) {
                Label::Unknown
            } else {
                match owned.owner {
                    Owner::Direct if !ambiguous => Label::AiDirect,
                    Owner::Direct if duplicate_candidates_share_ai_source(line, ev) => {
                        Label::AiStrong
                    }
                    Owner::Direct => Label::AiProbable,
                    Owner::Ai if !ambiguous || duplicate_candidates_share_ai_source(line, ev) => {
                        Label::AiStrong
                    }
                    Owner::Ai | Owner::AiWeak => Label::AiProbable,
                    Owner::AiEdited => Label::Mixed,
                    Owner::Base | Owner::External | Owner::Gap => Label::Unknown,
                }
            };
            AttributedLine {
                line: LineLabel {
                    line: index + 1,
                    label,
                    added: added[index],
                    blank: line.iter().all(u8::is_ascii_whitespace),
                },
                source_step: owned.source_step,
                ambiguous,
                unobserved_edit: None,
            }
        })
        .collect()
}

fn duplicate_candidates_share_ai_source(line: &[u8], evidence: &Evidence) -> bool {
    let mut source = None;
    let mut found = false;
    for step in &evidence.steps {
        if !matches!(step.kind, StepKind::Ai { .. })
            || !split_lines(&step.after)
                .iter()
                .any(|candidate| candidate == line)
        {
            continue;
        }
        let Some(attribution) = step.attribution.as_ref() else {
            return false;
        };
        let (Some(adapter), Some(tool), Some(session), Some(operation)) = (
            attribution.adapter_id.as_deref(),
            attribution.tool_id.as_deref(),
            attribution.session_id.as_deref(),
            attribution.operation.as_deref(),
        ) else {
            return false;
        };
        let identity = (adapter, tool, session, operation);
        if source.is_some_and(|previous| previous != identity) {
            return false;
        }
        source = Some(identity);
        found = true;
    }
    found
}

/// Reconcile an unobserved final edit against the last captured AI snapshot.
/// The caller uses this only when no later observed non-AI step supersedes that snapshot.
pub fn derive_lines_with_unobserved_edits(ev: &Evidence, committed: &[u8]) -> Vec<AttributedLine> {
    let Some(last_ai_step) = ev
        .steps
        .iter()
        .rposition(|step| matches!(step.kind, StepKind::Ai { .. }))
    else {
        return derive_lines_with_attribution(ev, committed);
    };

    let parent = split_lines(&ev.parent);
    let mut previous = parent.clone();
    let mut owners = vec![
        OwnedLine {
            owner: Owner::Base,
            candidates: owner_candidate(Owner::Base),
            source_step: None,
            ambiguous: false,
        };
        parent.len()
    ];
    for (step_index, step) in ev.steps.iter().enumerate().take(last_ai_step + 1) {
        let next = split_lines(&step.after);
        let direct_lines = step
            .attribution
            .as_ref()
            .map(|attribution| attribution.direct_lines.as_slice())
            .unwrap_or_default();
        owners = replay(
            &previous,
            &owners,
            &next,
            step.kind,
            step_index,
            direct_lines,
        );
        previous = next;
    }

    let snapshot = split_lines(&ev.steps[last_ai_step].after);
    let committed_lines = split_lines(committed);
    let ops = capture_diff_slices(Algorithm::Myers, &snapshot, &committed_lines);
    let has_unmatched_changes = ops.iter().any(|op| op.tag() != DiffTag::Equal);
    let added = added_lines(&parent, &committed_lines);
    let mut snapshot_counts = HashMap::<Vec<u8>, usize>::new();
    for line in &snapshot {
        *snapshot_counts.entry(line.clone()).or_default() += 1;
    }
    let mut committed_owners = vec![
        OwnedLine {
            owner: Owner::Gap,
            candidates: owner_candidate(Owner::Gap),
            source_step: None,
            ambiguous: false,
        };
        committed_lines.len()
    ];
    let mut unobserved_edits = vec![None; committed_lines.len()];
    let mut cursor = 0;
    while cursor < ops.len() {
        if ops[cursor].tag() == DiffTag::Equal {
            for (old_index, new_index) in ops[cursor].old_range().zip(ops[cursor].new_range()) {
                let owner = owners[old_index];
                if is_ai_ancestry(owner.owner) && has_unmatched_changes {
                    unobserved_edits[new_index] =
                        Some(UnobservedEditKind::ExactSurvivorInChangedFile);
                }
                committed_owners[new_index] = owner;
            }
            cursor += 1;
            continue;
        }

        let start = cursor;
        while cursor < ops.len() && ops[cursor].tag() != DiffTag::Equal {
            cursor += 1;
        }
        let old_indices: Vec<_> = ops[start..cursor]
            .iter()
            .flat_map(|op| op.old_range())
            .filter(|index| is_ai_ancestry(owners[*index].owner))
            .collect();
        let new_indices: Vec<_> = ops[start..cursor]
            .iter()
            .flat_map(|op| op.new_range())
            .collect();
        let mut matched_new = HashSet::new();
        let mut remaining_old = Vec::new();
        let mut next_new = 0;
        for old_index in old_indices {
            let Some(relative_new) = new_indices[next_new..]
                .iter()
                .position(|&new_index| snapshot[old_index] == committed_lines[new_index])
            else {
                remaining_old.push(old_index);
                continue;
            };
            let new_position = next_new + relative_new;
            let new_index = new_indices[new_position];
            next_new = new_position + 1;
            matched_new.insert(new_index);
            committed_owners[new_index] = owners[old_index];
            unobserved_edits[new_index] = Some(UnobservedEditKind::ExactSurvivorInChangedFile);
        }
        let remaining_new: Vec<_> = new_indices
            .into_iter()
            .filter(|new_index| !matched_new.contains(new_index))
            .collect();
        for &new_index in &remaining_new {
            unobserved_edits[new_index] = Some(UnobservedEditKind::UnmatchedUnknown);
        }
        for (old_index, new_index) in remaining_old.into_iter().zip(remaining_new) {
            let old_line = &snapshot[old_index];
            let new_line = &committed_lines[new_index];
            if sufficiently_informative(old_line)
                && sufficiently_informative(new_line)
                && token_overlap(old_line, new_line) >= MIN_MIXED_OVERLAP
            {
                let source = owners[old_index];
                committed_owners[new_index] = OwnedLine {
                    owner: Owner::AiEdited,
                    candidates: source.candidates | owner_candidate(Owner::AiEdited),
                    source_step: source.source_step,
                    ambiguous: source.ambiguous,
                };
                unobserved_edits[new_index] = Some(UnobservedEditKind::MixedByTokenOverlap);
            }
        }
    }

    let mut committed_counts = HashMap::<Vec<u8>, usize>::new();
    for line in &committed_lines {
        *committed_counts.entry(line.clone()).or_default() += 1;
    }
    let mut committed_occurrences = HashMap::<Vec<u8>, usize>::new();
    for (index, line) in committed_lines.iter().enumerate() {
        let occurrence = committed_occurrences.entry(line.clone()).or_default();
        *occurrence += 1;
        let captured = snapshot_counts.get(line).copied().unwrap_or_default();
        if captured == 1 && committed_counts.get(line).copied().unwrap_or_default() > 1 {
            committed_owners[index].candidates |= owner_candidate(Owner::Gap);
            committed_owners[index].ambiguous = true;
        }
        if *occurrence > captured && captured > 0 {
            committed_owners[index] = OwnedLine {
                owner: Owner::Gap,
                candidates: owner_candidate(Owner::Gap),
                source_step: None,
                ambiguous: false,
            };
            unobserved_edits[index] = Some(UnobservedEditKind::UnmatchedUnknown);
        }
    }

    let duplicate_owners = duplicate_owners(&committed_owners);

    committed_lines
        .iter()
        .enumerate()
        .map(|(index, line)| {
            let owned = committed_owners[index];
            let duplicates = duplicate_owners[index];
            let candidates = owned.candidates | duplicates.candidates;
            let ambiguous = owned.ambiguous || duplicates.ambiguous;
            let label = if !added[index] || !all_candidates_have_ai_ancestry(candidates) {
                Label::Unknown
            } else {
                match owned.owner {
                    Owner::Direct if !ambiguous => Label::AiDirect,
                    Owner::Direct => Label::AiProbable,
                    Owner::Ai if !ambiguous => Label::AiStrong,
                    Owner::Ai | Owner::AiWeak => Label::AiProbable,
                    Owner::AiEdited => Label::Mixed,
                    Owner::Base | Owner::External | Owner::Gap => Label::Unknown,
                }
            };
            AttributedLine {
                line: LineLabel {
                    line: index + 1,
                    label,
                    added: added[index],
                    blank: line.iter().all(u8::is_ascii_whitespace),
                },
                source_step: owned.source_step,
                ambiguous,
                unobserved_edit: unobserved_edits[index],
            }
        })
        .collect()
}

/// Return AI-owned snapshot lines removed in each changed hunk, with nonblank additions
/// from the committed file. Callers pair these with the shared deterministic matcher.
pub fn unobserved_replacement_hunks(
    ev: &Evidence,
    committed: &[u8],
) -> Vec<UnobservedReplacementHunk> {
    let Some(last_ai_step) = ev
        .steps
        .iter()
        .rposition(|step| matches!(step.kind, StepKind::Ai { .. }))
    else {
        return Vec::new();
    };
    let snapshot = &ev.steps[last_ai_step].after;
    let snapshot_attribution = derive_lines_with_attribution(
        &Evidence {
            parent: ev.parent.clone(),
            steps: ev.steps[..=last_ai_step].to_vec(),
        },
        snapshot,
    );
    let snapshot_lines = split_lines(snapshot);
    let committed_lines = split_lines(committed);
    let operations = capture_diff_slices(Algorithm::Myers, &snapshot_lines, &committed_lines);
    let mut hunks = Vec::new();
    let mut cursor = 0;
    while cursor < operations.len() {
        if operations[cursor].tag() == DiffTag::Equal {
            cursor += 1;
            continue;
        }
        let start = cursor;
        while cursor < operations.len() && operations[cursor].tag() != DiffTag::Equal {
            cursor += 1;
        }
        let removed_ai = operations[start..cursor]
            .iter()
            .flat_map(|operation| operation.old_range())
            .filter_map(|line_index| {
                let attributed = snapshot_attribution.get(line_index)?;
                matches!(
                    attributed.line.label,
                    Label::AiStrong | Label::AiProbable | Label::Mixed
                )
                .then(|| UnobservedAiLine {
                    text: snapshot_lines[line_index].clone(),
                    source_step: attributed.source_step,
                    label: attributed.line.label,
                })
            })
            .collect::<Vec<_>>();
        let added_line_indices = operations[start..cursor]
            .iter()
            .flat_map(|operation| operation.new_range())
            .filter(|&line_index| {
                !committed_lines[line_index]
                    .iter()
                    .all(u8::is_ascii_whitespace)
            })
            .collect::<Vec<_>>();
        if !removed_ai.is_empty() && !added_line_indices.is_empty() {
            hunks.push(UnobservedReplacementHunk {
                removed_ai,
                added_line_indices,
            });
        }
    }
    hunks
}

fn is_ai_ancestry(owner: Owner) -> bool {
    matches!(owner, Owner::Ai | Owner::AiWeak | Owner::AiEdited)
}

fn sufficiently_informative(line: &[u8]) -> bool {
    String::from_utf8_lossy(line).trim().chars().count() >= 8
}

fn token_overlap(left: &[u8], right: &[u8]) -> f64 {
    let left_tokens = lexical_tokens(left);
    let right_tokens = lexical_tokens(right);
    if left_tokens.is_empty() || right_tokens.is_empty() {
        return 0.0;
    }
    let mut counts = HashMap::<Vec<u8>, usize>::new();
    for token in &left_tokens {
        *counts.entry(token.clone()).or_default() += 1;
    }
    let shared: usize = {
        let mut remaining = counts;
        let mut shared = 0;
        for token in &right_tokens {
            if let Some(count) = remaining.get_mut(token) {
                if *count > 0 {
                    *count -= 1;
                    shared += 1;
                }
            }
        }
        shared
    };
    shared as f64 / left_tokens.len().max(right_tokens.len()) as f64
}

fn lexical_tokens(line: &[u8]) -> Vec<Vec<u8>> {
    let mut tokens = Vec::new();
    let mut index = 0;
    while index < line.len() {
        let byte = line[index];
        if byte.is_ascii_whitespace() {
            index += 1;
        } else if byte.is_ascii_alphanumeric() || byte == b'_' {
            let start = index;
            index += 1;
            while index < line.len() && (line[index].is_ascii_alphanumeric() || line[index] == b'_')
            {
                index += 1;
            }
            tokens.push(line[start..index].to_vec());
        } else if is_operator(byte) {
            let start = index;
            index += 1;
            while index < line.len() && is_operator(line[index]) {
                index += 1;
            }
            tokens.push(line[start..index].to_vec());
        } else {
            index += 1;
        }
    }
    tokens
}

fn is_operator(byte: u8) -> bool {
    matches!(
        byte,
        b'!' | b'%'
            | b'&'
            | b'*'
            | b'+'
            | b'-'
            | b'/'
            | b':'
            | b'<'
            | b'='
            | b'>'
            | b'?'
            | b'^'
            | b'|'
            | b'~'
    )
}

pub fn count(labels: &[LineLabel]) -> Counts {
    let mut counts = Counts::default();
    for line in labels.iter().filter(|line| line.added && !line.blank) {
        counts.e += 1;
        match line.label {
            Label::AiDirect => counts.d += 1,
            Label::AiStrong => counts.s += 1,
            Label::AiProbable => counts.p += 1,
            Label::Mixed => counts.m += 1,
            Label::Unknown => counts.u += 1,
        }
    }
    counts
}

fn split_lines(bytes: &[u8]) -> Vec<Vec<u8>> {
    if bytes.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<Vec<u8>> = bytes
        .split(|byte| *byte == b'\n')
        .map(<[u8]>::to_vec)
        .collect();
    if bytes.last() == Some(&b'\n') {
        lines.pop();
    }
    lines
}

fn replay(
    old: &[Vec<u8>],
    owners: &[OwnedLine],
    new: &[Vec<u8>],
    kind: StepKind,
    source_step: usize,
    direct_lines: &[Vec<u8>],
) -> Vec<OwnedLine> {
    let ops = capture_diff_slices(Algorithm::Myers, old, new);
    let mut next_owners = vec![
        OwnedLine {
            owner: Owner::Gap,
            candidates: owner_candidate(Owner::Gap),
            source_step: None,
            ambiguous: false,
        };
        new.len()
    ];
    let mut cursor = 0;

    while cursor < ops.len() {
        if ops[cursor].tag() == DiffTag::Equal {
            let old_range = ops[cursor].old_range();
            let new_range = ops[cursor].new_range();
            for (old_index, new_index) in old_range.zip(new_range) {
                next_owners[new_index] = owners[old_index];
            }
            cursor += 1;
            continue;
        }

        let start = cursor;
        while cursor < ops.len() && ops[cursor].tag() != DiffTag::Equal {
            cursor += 1;
        }
        let mut deleted_ai_sources = HashSet::new();
        let mut deleted_ai = false;
        let mut deleted_candidates = 0;
        let mut deleted_count = 0;
        for op in &ops[start..cursor] {
            for old_index in op.old_range() {
                deleted_count += 1;
                let old_owner = owners[old_index];
                deleted_candidates |= old_owner.candidates;
                if matches!(
                    old_owner.owner,
                    Owner::Ai | Owner::Direct | Owner::AiWeak | Owner::AiEdited
                ) {
                    deleted_ai = true;
                    if let Some(step) = old_owner.source_step {
                        deleted_ai_sources.insert(step);
                    }
                }
            }
        }
        let inserted_count = ops[start..cursor]
            .iter()
            .map(|op| op.new_range().len())
            .sum::<usize>();
        let inserted_owner = match kind {
            StepKind::Ai { degraded: false } => OwnedLine {
                owner: if direct_lines.is_empty() {
                    Owner::Ai
                } else {
                    Owner::Direct
                },
                candidates: if direct_lines.is_empty() {
                    owner_candidate(Owner::Ai)
                } else {
                    owner_candidate(Owner::Direct)
                },
                source_step: Some(source_step),
                ambiguous: false,
            },
            StepKind::Ai { degraded: true } => OwnedLine {
                owner: Owner::AiWeak,
                candidates: owner_candidate(Owner::AiWeak),
                source_step: Some(source_step),
                ambiguous: false,
            },
            StepKind::External if deleted_ai => OwnedLine {
                owner: Owner::AiEdited,
                candidates: owner_candidate(Owner::AiEdited)
                    | deleted_candidates
                    | if deleted_count != inserted_count {
                        owner_candidate(Owner::External)
                    } else {
                        0
                    },
                source_step: (deleted_ai_sources.len() == 1)
                    .then(|| *deleted_ai_sources.iter().next().unwrap()),
                ambiguous: deleted_ai_sources.len() > 1,
            },
            StepKind::External => OwnedLine {
                owner: Owner::External,
                candidates: owner_candidate(Owner::External),
                source_step: Some(source_step),
                ambiguous: false,
            },
            StepKind::Unknown => OwnedLine {
                owner: Owner::Gap,
                candidates: owner_candidate(Owner::Gap),
                source_step: Some(source_step),
                ambiguous: false,
            },
        };
        for op in &ops[start..cursor] {
            for new_index in op.new_range() {
                next_owners[new_index] = inserted_owner;
            }
        }
    }
    // A step that inserts or deletes text identical to a carried AI line makes the match
    // ambiguous (the diff may have paired the wrong copy), so that carried line is downgraded.
    let mut touched = HashSet::<&[u8]>::new();
    let mut inserted = Vec::<&[u8]>::new();
    for op in ops.iter().filter(|op| op.tag() != DiffTag::Equal) {
        touched.extend(old[op.old_range()].iter().map(Vec::as_slice));
        for line in &new[op.new_range()] {
            touched.insert(line.as_slice());
            inserted.push(line.as_slice());
        }
    }
    if !touched.is_empty() {
        for op in ops.iter().filter(|op| op.tag() == DiffTag::Equal) {
            for (old_index, new_index) in op.old_range().zip(op.new_range()) {
                let line = old[old_index].as_slice();
                if !touched.remove(line) {
                    continue;
                }
                let old_count = old
                    .iter()
                    .filter(|candidate| candidate.as_slice() == line)
                    .count();
                let new_count = new
                    .iter()
                    .filter(|candidate| candidate.as_slice() == line)
                    .count();
                let mut provenance = None;
                let conflicting_provenance = old
                    .iter()
                    .zip(owners)
                    .filter(|(candidate, _)| candidate.as_slice() == line)
                    .any(|(_, owner)| {
                        let current = (owner.owner, owner.source_step, owner.candidates);
                        provenance
                            .replace(current)
                            .is_some_and(|previous| previous != current)
                    });
                let equal_old_count = ops
                    .iter()
                    .filter(|operation| operation.tag() == DiffTag::Equal)
                    .flat_map(|operation| operation.old_range().zip(operation.new_range()))
                    .filter(|(old_index, new_index)| {
                        old[*old_index].as_slice() == line && new[*new_index].as_slice() == line
                    })
                    .count();
                let all_old_occurrences_direct = old
                    .iter()
                    .zip(owners)
                    .filter(|(candidate, _)| candidate.as_slice() == line)
                    .all(|(_, owner)| owner.owner == Owner::Direct);
                if old_count > 1
                    && new_count >= old_count
                    && equal_old_count == old_count
                    && !conflicting_provenance
                    && all_old_occurrences_direct
                    && matches!(kind, StepKind::External)
                {
                    if new_count > old_count
                        && matches!(kind, StepKind::External | StepKind::Unknown)
                    {
                        let equal_new: HashSet<_> = ops
                            .iter()
                            .filter(|operation| operation.tag() == DiffTag::Equal)
                            .flat_map(|operation| operation.old_range().zip(operation.new_range()))
                            .filter_map(|(old_index, new_index)| {
                                (old[old_index].as_slice() == line
                                    && new[new_index].as_slice() == line)
                                    .then_some(new_index)
                            })
                            .collect();
                        for (index, candidate) in new.iter().enumerate() {
                            if candidate.as_slice() == line && !equal_new.contains(&index) {
                                next_owners[index] = match kind {
                                    StepKind::External => OwnedLine {
                                        owner: Owner::External,
                                        candidates: owner_candidate(Owner::External),
                                        source_step: Some(source_step),
                                        ambiguous: false,
                                    },
                                    StepKind::Unknown => OwnedLine {
                                        owner: Owner::Gap,
                                        candidates: owner_candidate(Owner::Gap),
                                        source_step: Some(source_step),
                                        ambiguous: false,
                                    },
                                    StepKind::Ai { .. } => unreachable!(),
                                };
                            }
                        }
                    }
                    continue;
                }
                let mut candidates = 0;
                for (old_line, owner) in old.iter().zip(owners) {
                    if old_line.as_slice() == line {
                        candidates |= owner.candidates;
                    }
                }
                if inserted.contains(&line) {
                    candidates |= match kind {
                        StepKind::Ai { .. } => {
                            owner_candidate(Owner::Ai) | owner_candidate(Owner::Direct)
                        }
                        StepKind::External => owner_candidate(Owner::External),
                        StepKind::Unknown => owner_candidate(Owner::Gap),
                    };
                }
                for (index, new_line) in new.iter().enumerate() {
                    if new_line.as_slice() == line {
                        next_owners[index].candidates |= candidates;
                        next_owners[index].ambiguous = true;
                    }
                }
                next_owners[new_index].candidates |= candidates;
            }
        }
    }
    next_owners
}

fn duplicate_owners(owners: &[OwnedLine]) -> Vec<DuplicateOwners> {
    owners
        .iter()
        .map(|owner| DuplicateOwners {
            candidates: owner.candidates,
            ambiguous: owner.ambiguous,
        })
        .collect()
}

fn added_lines(parent: &[Vec<u8>], committed: &[Vec<u8>]) -> Vec<bool> {
    let ops = capture_diff_slices(Algorithm::Myers, parent, committed);
    let mut added = vec![false; committed.len()];
    for op in ops.iter().filter(|op| op.tag() != DiffTag::Equal) {
        for new_index in op.new_range() {
            added[new_index] = true;
        }
    }
    added
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(kind: StepKind, after: &[u8]) -> Step {
        Step {
            kind,
            after: after.to_vec(),
            attribution: None,
        }
    }

    fn sourced_ai_step(after: &[u8], adapter: &str, tool: &str, session: &str) -> Step {
        let mut result = step(StepKind::Ai { degraded: false }, after);
        result.attribution = Some(StepAttribution {
            adapter_id: Some(adapter.into()),
            tool_id: Some(adapter.into()),
            run_id: Some(format!("invocation-{}", after.len())),
            session_id: Some(session.into()),
            operation: Some(tool.into()),
            primary_basis: None,
            score_milli: 900,
            penalties_milli: Vec::new(),
            supporting_basis: Vec::new(),
            limiting_basis: Vec::new(),
            direct_lines: Vec::new(),
        });
        result
    }

    fn evidence(parent: &[u8], steps: Vec<Step>) -> Evidence {
        Evidence {
            parent: parent.to_vec(),
            steps,
        }
    }

    fn only_label(ev: &Evidence, committed: &[u8]) -> Label {
        derive_lines(ev, committed)
            .into_iter()
            .find(|line| line.added && !line.blank)
            .unwrap()
            .label
    }

    #[test]
    fn current_owner_transitions_keep_the_existing_categories() {
        let cases = [
            (
                evidence(
                    b"base\n",
                    vec![step(StepKind::Ai { degraded: false }, b"base\nadded\n")],
                ),
                b"base\nadded\n".as_slice(),
                Label::AiStrong,
            ),
            (
                evidence(
                    b"base\n",
                    vec![step(StepKind::Ai { degraded: true }, b"base\nadded\n")],
                ),
                b"base\nadded\n".as_slice(),
                Label::AiProbable,
            ),
            (
                evidence(
                    b"base\n",
                    vec![
                        step(StepKind::Ai { degraded: false }, b"base\nai\n"),
                        step(StepKind::External, b"base\nexternal replacement\n"),
                    ],
                ),
                b"base\nexternal replacement\n".as_slice(),
                Label::Mixed,
            ),
            (
                evidence(
                    b"base\n",
                    vec![step(StepKind::External, b"base\nexternal\n")],
                ),
                b"base\nexternal\n".as_slice(),
                Label::Unknown,
            ),
            (
                evidence(b"base\n", vec![step(StepKind::Unknown, b"base\nunbound\n")]),
                b"base\nunbound\n".as_slice(),
                Label::Unknown,
            ),
            (
                evidence(
                    b"base\n",
                    vec![
                        step(StepKind::Ai { degraded: false }, b"base\nai\n"),
                        step(StepKind::External, b"base\n"),
                        step(StepKind::Unknown, b"base\nreintroduced\n"),
                    ],
                ),
                b"base\nreintroduced\n".as_slice(),
                Label::Unknown,
            ),
            (
                evidence(
                    b"base\n",
                    vec![
                        step(StepKind::Ai { degraded: false }, b"base\nai\n"),
                        step(StepKind::External, b"base\nreplacement\n"),
                        step(StepKind::External, b"base\nsecond replacement\n"),
                    ],
                ),
                b"base\nsecond replacement\n".as_slice(),
                Label::Mixed,
            ),
        ];
        for (evidence, committed, expected) in cases {
            assert_eq!(only_label(&evidence, committed), expected);
        }
    }

    #[test]
    fn duplicate_ambiguity_is_probable_only_across_ai_sources() {
        let touched_duplicate = evidence(
            b"",
            vec![
                step(StepKind::Ai { degraded: false }, b"same\n"),
                step(StepKind::External, b"same\nsame\n"),
            ],
        );
        assert_eq!(
            only_label(&touched_duplicate, b"same\nsame\n"),
            Label::Unknown
        );

        let ai_only_duplicate = evidence(
            b"",
            vec![
                sourced_ai_step(b"same\n", "vscode-copilot", "create_file", "session-a"),
                sourced_ai_step(
                    b"same\nsame\n",
                    "vscode-copilot",
                    "create_file",
                    "session-a",
                ),
            ],
        );
        let labels = derive_lines(&ai_only_duplicate, b"same\nsame\n");
        assert_eq!(labels[0].label, Label::AiStrong);
        assert_eq!(labels[1].label, Label::AiStrong);

        let mixed_tool_duplicate = evidence(
            b"",
            vec![
                sourced_ai_step(b"same\n", "vscode-copilot", "create_file", "session-a"),
                sourced_ai_step(b"same\nsame\n", "codex", "create_file", "session-a"),
            ],
        );
        let labels = derive_lines(&mixed_tool_duplicate, b"same\nsame\n");
        assert_eq!(labels[0].label, Label::AiProbable);
        assert_eq!(labels[1].label, Label::AiProbable);

        let mixed_session_duplicate = evidence(
            b"",
            vec![
                sourced_ai_step(b"same\n", "vscode-copilot", "create_file", "session-a"),
                sourced_ai_step(
                    b"same\nsame\n",
                    "vscode-copilot",
                    "create_file",
                    "session-b",
                ),
            ],
        );
        let labels = derive_lines(&mixed_session_duplicate, b"same\nsame\n");
        assert_eq!(labels[0].label, Label::AiProbable);
        assert_eq!(labels[1].label, Label::AiProbable);

        let mut direct_first =
            sourced_ai_step(b"same\n", "vscode-copilot", "create_file", "session-a");
        direct_first.attribution.as_mut().unwrap().direct_lines = vec![b"same".to_vec()];
        let mut direct_second = sourced_ai_step(
            b"same\nsame\n",
            "vscode-copilot",
            "create_file",
            "session-a",
        );
        direct_second.attribution.as_mut().unwrap().direct_lines = vec![b"same".to_vec()];
        let direct_duplicate = evidence(b"", vec![direct_first, direct_second]);
        assert!(derive_lines(&direct_duplicate, b"same\nsame\n")
            .iter()
            .all(|line| line.label == Label::AiStrong));

        let conflicting_committed_owners = evidence(
            b"base\n",
            vec![
                step(StepKind::Ai { degraded: false }, b"base\nsame\n"),
                step(StepKind::External, b"base\nsame\nsame\n"),
            ],
        );
        let labels = derive_lines(&conflicting_committed_owners, b"base\nsame\nsame\n");
        assert_eq!(labels[1].label, Label::Unknown);
        assert_eq!(labels[2].label, Label::Unknown);

        let base_and_ai_candidates = evidence(
            b"same\nsame\n",
            vec![step(
                StepKind::Ai { degraded: false },
                b"same\nsame\nsame\n",
            )],
        );
        let labels = derive_lines(&base_and_ai_candidates, b"same\nsame\nsame\n");
        assert!(labels
            .iter()
            .filter(|line| line.added)
            .all(|line| { line.label == Label::Unknown }));
    }

    #[test]
    fn multi_create_fixture_keeps_same_session_duplicate_lines_strong() {
        let first = include_bytes!("../tests/fixtures/multi_create/first.ts");
        let second = include_bytes!("../tests/fixtures/multi_create/second.ts");
        let third = include_bytes!("../tests/fixtures/multi_create/third.ts");
        let committed = include_bytes!("../tests/fixtures/multi_create/committed.ts");
        let events = include_str!("../tests/fixtures/multi_create/events.jsonl")
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect::<Vec<_>>();
        let source = |snapshot: &[u8], event: &serde_json::Value| {
            let mut result = sourced_ai_step(
                snapshot,
                event["tool"].as_str().unwrap(),
                event["operation"].as_str().unwrap(),
                event["session_id"].as_str().unwrap(),
            );
            let attribution = result.attribution.as_mut().unwrap();
            attribution.run_id = event["tool_use_id"].as_str().map(str::to_owned);
            result
        };
        let steps = [&first[..], &second[..], &third[..]]
            .into_iter()
            .zip(&events)
            .map(|(snapshot, event)| source(snapshot, event))
            .collect();
        let ev = evidence(b"", steps);

        let labels = derive_lines_with_attribution(&ev, committed);
        assert!(labels.iter().any(|line| line.ambiguous));
        let ambiguous_ai = labels
            .iter()
            .filter(|line| {
                line.ambiguous
                    && line.line.added
                    && !line.line.blank
                    && line.line.label != Label::Unknown
            })
            .collect::<Vec<_>>();
        assert!(!ambiguous_ai.is_empty());
        assert!(ambiguous_ai
            .iter()
            .all(|line| line.line.label == Label::AiStrong));
    }

    #[test]
    fn pure_ai_add_is_strong_and_pure_human_add_is_unknown() {
        let ai = evidence(
            b"base\n",
            vec![step(StepKind::Ai { degraded: false }, b"base\nai\n")],
        );
        let human = evidence(b"base\n", vec![step(StepKind::External, b"base\nhuman\n")]);
        assert_eq!(only_label(&ai, b"base\nai\n"), Label::AiStrong);
        assert_eq!(only_label(&human, b"base\nhuman\n"), Label::Unknown);
    }

    #[test]
    fn human_replacement_of_ai_is_mixed_and_ai_replacement_of_human_is_strong() {
        let ai_then_human = evidence(
            b"base\n",
            vec![
                step(StepKind::Ai { degraded: false }, b"base\nai\n"),
                step(StepKind::External, b"base\nhuman\n"),
            ],
        );
        let human_then_ai = evidence(
            b"base\n",
            vec![
                step(StepKind::External, b"base\nhuman\n"),
                step(StepKind::Ai { degraded: false }, b"base\nai\n"),
            ],
        );
        assert_eq!(only_label(&ai_then_human, b"base\nhuman\n"), Label::Mixed);
        assert_eq!(only_label(&human_then_ai, b"base\nai\n"), Label::AiStrong);
    }

    #[test]
    fn degraded_ai_is_probable_and_unknown_step_stays_unknown() {
        let degraded = evidence(b"", vec![step(StepKind::Ai { degraded: true }, b"weak\n")]);
        let unknown = evidence(b"", vec![step(StepKind::Unknown, b"unknown\n")]);
        assert_eq!(only_label(&degraded, b"weak\n"), Label::AiProbable);
        assert_eq!(only_label(&unknown, b"unknown\n"), Label::Unknown);
    }

    #[test]
    fn lines_after_last_step_are_unknown() {
        let ev = evidence(b"", vec![step(StepKind::Ai { degraded: false }, b"seen\n")]);
        let labels = derive_lines(&ev, b"seen\nafter\n");
        assert_eq!(labels[1].label, Label::Unknown);
    }

    #[test]
    fn blank_lines_are_reported_but_excluded_from_counts() {
        let ev = evidence(
            b"",
            vec![step(StepKind::Ai { degraded: false }, b"\n  \ntext\n")],
        );
        let labels = derive_lines(&ev, b"\n  \ntext\n");
        assert!(labels[0].blank && labels[1].blank);
        assert_eq!(
            count(&labels),
            Counts {
                d: 0,
                s: 1,
                p: 0,
                m: 0,
                u: 0,
                e: 1
            }
        );
        assert_eq!(
            count(&labels).e,
            count(&labels).s + count(&labels).p + count(&labels).m + count(&labels).u
        );
    }

    #[test]
    fn deleted_then_retyped_lines_use_the_retyping_owner() {
        let ev = evidence(
            b"",
            vec![
                step(StepKind::Ai { degraded: false }, b"same\n"),
                step(StepKind::External, b""),
                step(StepKind::Unknown, b"same\n"),
            ],
        );
        assert_eq!(only_label(&ev, b"same\n"), Label::Unknown);
    }

    #[test]
    fn crlf_and_non_utf8_bytes_are_preserved() {
        let crlf = evidence(
            b"",
            vec![step(StepKind::Ai { degraded: false }, b"line\r\n")],
        );
        let binary_text = evidence(
            b"",
            vec![step(StepKind::Ai { degraded: false }, b"\xff\xfe\n")],
        );
        assert_eq!(derive_lines(&crlf, b"line\r\n")[0].label, Label::AiStrong);
        assert_eq!(
            derive_lines(&binary_text, b"\xff\xfe\n")[0].label,
            Label::AiStrong
        );
    }

    #[test]
    fn empty_parent_and_committed_content_have_no_lines() {
        assert!(derive_lines(&evidence(b"", Vec::new()), b"").is_empty());
        assert!(derive_lines(&evidence(b"parent\n", Vec::new()), b"").is_empty());
    }

    #[test]
    fn identical_committed_text_is_conservatively_downgraded() {
        let ev = evidence(
            b"",
            vec![
                step(StepKind::Ai { degraded: false }, b"same\n"),
                step(StepKind::External, b"same\nsame\n"),
            ],
        );
        assert!(derive_lines(&ev, b"same\nsame\n")
            .iter()
            .all(|line| line.label != Label::AiStrong));
    }

    #[test]
    fn repeated_calls_are_deterministic() {
        let ev = evidence(
            b"base\n",
            vec![
                step(StepKind::Ai { degraded: false }, b"base\na\nb\n"),
                step(StepKind::External, b"base\nb\na\n"),
            ],
        );
        assert_eq!(
            derive_lines(&ev, b"base\nb\na\n"),
            derive_lines(&ev, b"base\nb\na\n")
        );
    }

    #[test]
    fn unobserved_survival_derivation_is_deterministic() {
        let ev = evidence(
            b"base\n",
            vec![step(
                StepKind::Ai { degraded: false },
                b"base\nlet product = a * b;\n",
            )],
        );
        let committed = b"base\nlet product = a * c;\n";
        assert_eq!(
            derive_lines_with_unobserved_edits(&ev, committed),
            derive_lines_with_unobserved_edits(&ev, committed)
        );
    }

    #[derive(Clone, Copy)]
    struct XorShift(u64);

    impl XorShift {
        fn next(&mut self) -> u64 {
            let mut value = self.0;
            value ^= value << 13;
            value ^= value >> 7;
            value ^= value << 17;
            self.0 = value;
            value
        }

        fn below(&mut self, bound: usize) -> usize {
            (self.next() as usize) % bound
        }
    }

    #[derive(Clone, Copy)]
    enum Author {
        Base,
        Human,
        Ai,
    }

    fn random_text(rng: &mut XorShift, case: usize, edit: usize, tiny: bool) -> Vec<u8> {
        if tiny {
            format!("v{}", rng.below(6)).into_bytes()
        } else {
            format!("u-{case}-{edit}-{}", rng.next()).into_bytes()
        }
    }

    fn bytes_of(file: &[(Vec<u8>, Author, bool)]) -> Vec<u8> {
        let mut bytes = Vec::new();
        for (text, _, _) in file {
            bytes.extend_from_slice(text);
            bytes.push(b'\n');
        }
        bytes
    }

    fn fuzz_case(seed: u64, case: usize, tiny: bool, enforce_no_ai_ancestry: bool) {
        let mut rng = XorShift(seed | 1);
        let initial_count = rng.below(5);
        let mut file: Vec<(Vec<u8>, Author, bool)> = (0..initial_count)
            .map(|edit| (random_text(&mut rng, case, edit, tiny), Author::Base, false))
            .collect();
        let parent = bytes_of(&file);
        let mut steps = Vec::new();
        let edit_count = 2 + rng.below(9);

        for edit in 0..edit_count {
            let actor = if rng.below(2) == 0 {
                Author::Human
            } else {
                Author::Ai
            };
            let action = rng.below(3);
            if file.is_empty() || action == 0 {
                let text = random_text(&mut rng, case, edit, tiny);
                let index = rng.below(file.len() + 1);
                file.insert(index, (text, actor, matches!(actor, Author::Ai)));
            } else if action == 1 {
                file.remove(rng.below(file.len()));
            } else {
                let index = rng.below(file.len());
                let old_text = file[index].0.clone();
                let mut text = random_text(&mut rng, case, edit, tiny);
                while text == old_text {
                    text = random_text(&mut rng, case, edit, tiny);
                }
                let ai_ancestry = file[index].2 || matches!(actor, Author::Ai);
                file[index] = (text, actor, ai_ancestry);
            }
            steps.push(step(
                match actor {
                    Author::Ai => StepKind::Ai { degraded: false },
                    Author::Human => StepKind::External,
                    Author::Base => unreachable!(),
                },
                &bytes_of(&file),
            ));
        }

        let committed = bytes_of(&file);
        let labels = derive_lines(&evidence(&parent, steps), &committed);
        for ((_, author, ai_ancestry), label) in file.iter().zip(labels.iter()) {
            assert!(
                !matches!(author, Author::Human) || label.label != Label::AiStrong,
                "human line labeled strong in tiny={tiny}, case={case}, line={}: {:?}",
                label.line,
                label
            );
            if enforce_no_ai_ancestry {
                assert!(
                    *ai_ancestry
                        || !matches!(
                            label.label,
                            Label::AiStrong | Label::AiProbable | Label::Mixed
                        ),
                    "line without AI ancestry received {:?} in tiny={tiny}, case={case}, line={}: {:?}",
                    label.label,
                    label.line,
                    label
                );
            }
        }
    }

    #[test]
    fn fixed_seed_fuzz_never_labels_human_lines_ai_strong() {
        for tiny in [false, true] {
            for case in 0..3000 {
                fuzz_case(0x9e37_79b9_7f4a_7c15 ^ case as u64, case, tiny, false);
            }
        }
    }

    #[test]
    fn fixed_seed_fuzz_no_ai_category_on_lines_without_ai_ancestry() {
        for tiny in [false, true] {
            for case in 0..3000 {
                fuzz_case(0x9e37_79b9_7f4a_7c15 ^ case as u64, case, tiny, true);
            }
        }
    }

    #[test]
    fn human_copy_of_ai_text_then_ai_copy_removed_is_not_strong() {
        // Regression: the diff pairs the surviving human "v4" with the old AI "v4".
        let ev = evidence(
            b"v3\nv1\n",
            vec![
                step(StepKind::External, b"v0\nv1\n"),
                step(StepKind::Ai { degraded: false }, b"v0\nv4\n"),
                step(StepKind::External, b"v0\nv4\nv4\n"),
                step(StepKind::External, b"v0\nv4\nv3\n"),
            ],
        );
        let labels = derive_lines(&ev, b"v0\nv4\nv3\n");
        assert_ne!(labels[1].label, Label::AiStrong);
    }

    #[test]
    fn unobserved_replacements_need_minimum_overlap_and_ai_ancestry() {
        let ev = evidence(
            b"base\n",
            vec![step(
                StepKind::Ai { degraded: false },
                b"base\nreturn a * b;\nstable_value = 12;\n",
            )],
        );
        let lines = derive_lines_with_unobserved_edits(
            &ev,
            b"base\nreturn a * c;\nstable_value = 12;\nnew unrelated value\n",
        );
        assert_eq!(lines[1].line.label, Label::Mixed);
        assert_eq!(
            lines[1].unobserved_edit,
            Some(UnobservedEditKind::MixedByTokenOverlap)
        );
        assert_eq!(lines[2].line.label, Label::AiStrong);
        assert_eq!(
            lines[2].unobserved_edit,
            Some(UnobservedEditKind::ExactSurvivorInChangedFile)
        );
        assert_eq!(lines[3].line.label, Label::Unknown);
        assert_eq!(
            lines[3].unobserved_edit,
            Some(UnobservedEditKind::UnmatchedUnknown)
        );
        assert!(token_overlap(b"return a * b;", b"return a * c;") >= MIN_MIXED_OVERLAP);
        assert!(sufficiently_informative(b"return a * b;"));
        assert!(!sufficiently_informative(b"v4"));
    }

    #[test]
    fn duplicate_ai_snapshot_lines_from_one_source_stay_strong_after_alignment() {
        let ev = evidence(
            b"base\n",
            vec![step(
                StepKind::Ai { degraded: false },
                b"base\nrepeat_call();\nrepeat_call();\n",
            )],
        );
        let lines = derive_lines_with_unobserved_edits(
            &ev,
            b"base\nrepeat_call();\nrepeat_call();\nlate_value();\n",
        );
        assert_eq!(lines[1].line.label, Label::AiStrong);
        assert_eq!(lines[2].line.label, Label::AiStrong);
        assert_eq!(lines[3].line.label, Label::Unknown);
    }

    #[test]
    fn copied_ai_text_with_ambiguous_survival_is_unknown() {
        let ev = evidence(
            b"base\n",
            vec![step(
                StepKind::Ai { degraded: false },
                b"base\ncalculate_total = left_value + right_value;\n",
            )],
        );
        let lines = derive_lines_with_unobserved_edits(
            &ev,
            b"base\ncalculate_total = left_value + right_value;\ncalculate_total = left_value + right_value;\n",
        );
        assert_eq!(lines[1].line.label, Label::Unknown);
        assert_eq!(lines[2].line.label, Label::Unknown);
    }

    #[test]
    fn real_reading_level_append_and_single_edit_keep_ai_lines_aligned() {
        let snapshot =
            include_bytes!("../../prov-hook/tests/fixtures/real_reading_level/ai_snapshot.ts");
        let committed =
            include_bytes!("../../prov-hook/tests/fixtures/real_reading_level/committed.ts");
        let ev = evidence(b"", vec![step(StepKind::Ai { degraded: false }, snapshot)]);
        let lines = derive_lines_with_unobserved_edits(&ev, committed);
        let text = split_lines(committed);
        let edited_line = text
            .iter()
            .position(|line| line.windows(3).any(|window| window == b"206"))
            .unwrap();
        assert_eq!(lines[edited_line].line.label, Label::Mixed);
        assert_eq!(
            lines[edited_line].unobserved_edit,
            Some(UnobservedEditKind::MixedByTokenOverlap)
        );
        let ai_snapshot_lines = split_lines(snapshot);
        let appended_start = ai_snapshot_lines.len();
        assert!(lines[appended_start..]
            .iter()
            .all(|line| line.line.label == Label::Unknown));
        for attributed in lines.iter().take(appended_start) {
            if attributed.line.line == edited_line + 1 {
                continue;
            }
            if attributed.line.blank {
                continue;
            }
            assert_eq!(
                attributed.line.label,
                Label::AiStrong,
                "line {}",
                attributed.line.line
            );
            assert_eq!(
                attributed.unobserved_edit,
                Some(UnobservedEditKind::ExactSurvivorInChangedFile),
                "line {}",
                attributed.line.line
            );
        }
    }

    #[test]
    fn repeated_braces_align_in_order_when_a_function_is_appended() {
        let snapshot = b"function one() {\n}\nfunction two() {\n}\nvalue = 200;\n";
        let committed =
            b"function one() {\n}\nfunction two() {\n}\nvalue = 201;\nfunction user() {\n}\n";
        let ev = evidence(b"", vec![step(StepKind::Ai { degraded: false }, snapshot)]);
        let lines = derive_lines_with_unobserved_edits(&ev, committed);
        assert_eq!(lines[1].line.label, Label::AiStrong);
        assert_eq!(lines[3].line.label, Label::AiStrong);
        assert_eq!(lines[4].line.label, Label::Mixed);
        assert!(lines[5..]
            .iter()
            .all(|line| line.line.label == Label::Unknown));
    }

    #[test]
    fn real_sentence_split_external_snapshot_explains_unknown_and_mixed_lines() {
        let ai =
            include_bytes!("../../prov-hook/tests/fixtures/real_sentence_split/ai_snapshot.ts");
        let external = include_bytes!(
            "../../prov-hook/tests/fixtures/real_sentence_split/external_snapshot.ts"
        );
        let committed =
            include_bytes!("../../prov-hook/tests/fixtures/real_sentence_split/committed.ts");
        let normalize = |bytes: &[u8]| {
            String::from_utf8_lossy(bytes)
                .replace("\r\n", "\n")
                .into_bytes()
        };
        let ai = normalize(ai);
        let external = normalize(external);
        let committed = normalize(committed);
        assert_eq!(split_lines(&ai).len(), 167);
        assert_eq!(split_lines(&external).len(), 159);
        assert_eq!(split_lines(&committed).len(), 159);
        assert_eq!(external, committed);
        let mut ai_step = step(StepKind::Ai { degraded: false }, &ai);
        ai_step.attribution = Some(StepAttribution {
            adapter_id: Some("vscode-copilot".into()),
            tool_id: Some("vscode-copilot".into()),
            run_id: None,
            session_id: None,
            operation: Some("create_file".into()),
            primary_basis: Some("DIRECT_ACCEPTED_EDIT".into()),
            score_milli: 1000,
            penalties_milli: Vec::new(),
            supporting_basis: Vec::new(),
            limiting_basis: Vec::new(),
            direct_lines: split_lines(&ai),
        });
        let ev = evidence(b"", vec![ai_step, step(StepKind::External, &external)]);
        let lines = derive_lines_with_attribution(&ev, &committed);
        assert!(!lines[50].ambiguous);
        assert_ne!(lines[50].line.label, Label::Unknown);
        let counts = count(
            &lines
                .iter()
                .map(|line| line.line.clone())
                .collect::<Vec<_>>(),
        );
        assert_eq!(
            (counts.d, counts.s, counts.p, counts.m, counts.u, counts.e),
            (94, 0, 10, 5, 25, 134)
        );
        assert!(lines.iter().any(|line| line.line.label == Label::Unknown));
        let events =
            include_str!("../../prov-hook/tests/fixtures/real_sentence_split/events.jsonl");
        let external_event = events
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .find(|event| event["event"] == "external_change")
            .unwrap();
        assert!(external_event["line_ranges"].as_array().unwrap().len() > 1);
        assert_eq!(external_event["deleted_at"].as_array().unwrap().len(), 14);
    }

    #[test]
    fn ai_function_next_to_existing_braces_stays_mostly_strong() {
        let parent = b"fn a() {\n}\n";
        let after = b"fn a() {\n}\n\nfn b() {\n    let x = 1;\n}\n";
        let ev = evidence(parent, vec![step(StepKind::Ai { degraded: false }, after)]);
        let counts = count(&derive_lines(&ev, after));
        assert_eq!(counts.e, 3);
        assert!(
            counts.s >= 2,
            "realistic AI code must not be all probable: {counts:?}"
        );
    }

    fn stress_run(vocab: usize, cases: u64) -> (u64, u64, u64) {
        let (mut strong_leaks, mut linked_leaks, mut strong) = (0u64, 0u64, 0u64);
        for case in 0..cases {
            let mut rng = XorShift((0x00C0_FFEE ^ case.wrapping_mul(0x9e37_79b9_7f4a_7c15)) | 1);
            let mut counter = 0u64;
            let mut text = |rng: &mut XorShift| -> Vec<u8> {
                counter += 1;
                if vocab == 0 {
                    format!("u{case}-{counter}").into_bytes()
                } else {
                    format!("v{}", rng.below(vocab)).into_bytes()
                }
            };
            let initial = rng.below(6);
            let mut file: Vec<(Vec<u8>, Author, bool)> = (0..initial)
                .map(|_| (text(&mut rng), Author::Base, false))
                .collect();
            let parent = bytes_of(&file);
            let mut steps = Vec::new();
            for _ in 0..(2 + rng.below(24)) {
                let actor = if rng.below(2) == 0 {
                    Author::Human
                } else {
                    Author::Ai
                };
                match rng.below(5) {
                    0 | 1 => {
                        let t = text(&mut rng);
                        let at = rng.below(file.len() + 1);
                        file.insert(at, (t, actor, matches!(actor, Author::Ai)));
                    }
                    2 => {
                        if !file.is_empty() {
                            let at = rng.below(file.len());
                            file.remove(at);
                        }
                    }
                    3 => {
                        if !file.is_empty() {
                            let at = rng.below(file.len());
                            let mut t = text(&mut rng);
                            let mut guard = 0;
                            while t == file[at].0 && guard < 50 {
                                t = text(&mut rng);
                                guard += 1;
                            }
                            if t != file[at].0 {
                                let ai_ancestry = file[at].2 || matches!(actor, Author::Ai);
                                file[at] = (t, actor, ai_ancestry);
                            }
                        }
                    }
                    _ => {
                        let at = rng.below(file.len() + 1);
                        for k in 0..(2 + rng.below(3)) {
                            let t = text(&mut rng);
                            file.insert(at + k, (t, actor, matches!(actor, Author::Ai)));
                        }
                    }
                }
                let kind = match actor {
                    Author::Ai => StepKind::Ai { degraded: false },
                    _ => StepKind::External,
                };
                steps.push(step(kind, &bytes_of(&file)));
            }
            let committed = bytes_of(&file);
            let labels = derive_lines(&evidence(&parent, steps), &committed);
            for ((_, author, ai_ancestry), label) in file.iter().zip(labels.iter()) {
                if matches!(
                    label.label,
                    Label::AiStrong | Label::AiProbable | Label::Mixed
                ) && !ai_ancestry
                {
                    linked_leaks += 1;
                }
                if label.label == Label::AiStrong {
                    strong += 1;
                    if !matches!(author, Author::Ai) {
                        strong_leaks += 1;
                    }
                }
            }
        }
        (strong_leaks, linked_leaks, strong)
    }

    #[test]
    fn stress_never_labels_non_ai_lines_strong_across_vocabularies() {
        for vocab in [0usize, 2, 3, 6, 12] {
            let (strong_leaks, _, strong) = stress_run(vocab, 12_000);
            assert_eq!(
                strong_leaks, 0,
                "vocab={vocab}: {strong_leaks} non-AI lines labelled strong"
            );
            if vocab == 0 {
                assert!(strong > 0);
            }
        }
    }

    #[test]
    fn stress_no_ai_category_on_lines_without_ai_ancestry() {
        for vocab in [0usize, 2, 3, 6, 12] {
            let (_, linked_leaks, _) = stress_run(vocab, 12_000);
            assert_eq!(
                linked_leaks, 0,
                "vocab={vocab}: {linked_leaks} lines without AI ancestry received an AI category"
            );
        }
    }
}
