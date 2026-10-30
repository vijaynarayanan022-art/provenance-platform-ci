use similar::{capture_diff_slices, Algorithm, DiffTag};
use std::collections::HashMap;

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
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Evidence {
    pub parent: Vec<u8>,
    pub steps: Vec<Step>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Label {
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
    AiWeak,
    AiEdited,
}

pub fn derive_lines(ev: &Evidence, committed: &[u8]) -> Vec<LineLabel> {
    let parent = split_lines(&ev.parent);
    let mut ambiguous_history = has_duplicate_lines(&parent);
    let mut previous = parent.clone();
    let mut owners = vec![Owner::Base; previous.len()];

    for step in &ev.steps {
        let next = split_lines(&step.after);
        ambiguous_history |= has_duplicate_lines(&next);
        owners = replay(&previous, &owners, &next, step.kind);
        previous = next;
    }

    let committed_lines = split_lines(committed);
    ambiguous_history |= has_duplicate_lines(&committed_lines);
    let committed_owners = replay(&previous, &owners, &committed_lines, StepKind::Unknown);
    let added = added_lines(&parent, &committed_lines);
    let mut occurrences: HashMap<&[u8], usize> = HashMap::new();
    for line in &committed_lines {
        *occurrences.entry(line.as_slice()).or_default() += 1;
    }

    committed_lines
        .iter()
        .enumerate()
        .map(|(index, line)| {
            let label = if !added[index] {
                Label::Unknown
            } else {
                match committed_owners[index] {
                    Owner::Ai if !ambiguous_history && occurrences[line.as_slice()] == 1 => {
                        Label::AiStrong
                    }
                    Owner::Ai | Owner::AiWeak => Label::AiProbable,
                    Owner::AiEdited => Label::Mixed,
                    Owner::Base | Owner::External | Owner::Gap => Label::Unknown,
                }
            };
            LineLabel {
                line: index + 1,
                label,
                added: added[index],
                blank: line.iter().all(u8::is_ascii_whitespace),
            }
        })
        .collect()
}

fn has_duplicate_lines(lines: &[Vec<u8>]) -> bool {
    let mut seen = HashMap::with_capacity(lines.len());
    lines
        .iter()
        .any(|line| seen.insert(line.as_slice(), ()).is_some())
}

pub fn count(labels: &[LineLabel]) -> Counts {
    let mut counts = Counts::default();
    for line in labels.iter().filter(|line| line.added && !line.blank) {
        counts.e += 1;
        match line.label {
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

fn replay(old: &[Vec<u8>], owners: &[Owner], new: &[Vec<u8>], kind: StepKind) -> Vec<Owner> {
    let ops = capture_diff_slices(Algorithm::Myers, old, new);
    let mut next_owners = vec![Owner::Gap; new.len()];
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
        let mut deleted_ai = false;
        for op in &ops[start..cursor] {
            for old_index in op.old_range() {
                deleted_ai |= matches!(
                    owners[old_index],
                    Owner::Ai | Owner::AiWeak | Owner::AiEdited
                );
            }
        }
        let inserted_owner = match kind {
            StepKind::Ai { degraded: false } => Owner::Ai,
            StepKind::Ai { degraded: true } => Owner::AiWeak,
            StepKind::External if deleted_ai => Owner::AiEdited,
            StepKind::External => Owner::External,
            StepKind::Unknown => Owner::Gap,
        };
        for op in &ops[start..cursor] {
            for new_index in op.new_range() {
                next_owners[new_index] = inserted_owner;
            }
        }
    }
    next_owners
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
        }
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

    fn bytes_of(file: &[(Vec<u8>, Author)]) -> Vec<u8> {
        let mut bytes = Vec::new();
        for (text, _) in file {
            bytes.extend_from_slice(text);
            bytes.push(b'\n');
        }
        bytes
    }

    fn fuzz_case(seed: u64, case: usize, tiny: bool) {
        let mut rng = XorShift(seed | 1);
        let initial_count = rng.below(5);
        let mut file: Vec<(Vec<u8>, Author)> = (0..initial_count)
            .map(|edit| (random_text(&mut rng, case, edit, tiny), Author::Base))
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
                file.insert(index, (text, actor));
            } else if action == 1 {
                file.remove(rng.below(file.len()));
            } else {
                let index = rng.below(file.len());
                let old_text = file[index].0.clone();
                let mut text = random_text(&mut rng, case, edit, tiny);
                while text == old_text {
                    text = random_text(&mut rng, case, edit, tiny);
                }
                file[index] = (text, actor);
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
        for ((_, author), label) in file.iter().zip(labels.iter()) {
            assert!(
                !matches!(author, Author::Human) || label.label != Label::AiStrong,
                "human line labeled strong in tiny={tiny}, case={case}, line={}: {:?}",
                label.line,
                label
            );
        }
    }

    #[test]
    fn fixed_seed_fuzz_never_labels_human_lines_ai_strong() {
        for tiny in [false, true] {
            for case in 0..3000 {
                fuzz_case(0x9e37_79b9_7f4a_7c15 ^ case as u64, case, tiny);
            }
        }
    }
}
