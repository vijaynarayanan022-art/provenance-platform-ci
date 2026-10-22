use std::collections::{BTreeMap, BTreeSet};

fn parse_range(s: &str) -> Option<(u32, u32)> {
    // "12,3" -> (12,3); "12" -> (12,1)
    match s.split_once(',') {
        Some((a, b)) => Some((a.parse().ok()?, b.parse().ok()?)),
        None => Some((s.parse().ok()?, 1)),
    }
}

fn parse_hunk_header(line: &str) -> Option<((u32, u32), (u32, u32))> {
    // "@@ -3,0 +4,6 @@ optional context"
    let rest = line.strip_prefix("@@ -")?;
    let (old, rest) = rest.split_once(" +")?;
    let (new, _) = rest.split_once(" @@")?;
    Some((parse_range(old)?, parse_range(new)?))
}

fn clean_path(raw: &str) -> Option<String> {
    let raw = raw.trim_end_matches('\t').trim();
    if raw == "/dev/null" {
        return None;
    }
    let unq = raw.strip_prefix('"').and_then(|r| r.strip_suffix('"')).unwrap_or(raw);
    Some(unq.strip_prefix("b/").unwrap_or(unq).to_string())
}

/// Parse `git diff-tree -p --unified=0` (or `git diff --unified=0`) output into
/// path -> set of 1-based line numbers ADDED in the new version of the file.
///
/// Uses the line counts in each hunk header, so file headers (`+++ b/x`) can never be
/// mistaken for added lines. Binary files have no hunks and yield nothing.
/// Limitation: run git with `-c core.quotepath=false`; escaped (quoted) paths are not decoded.
pub fn parse_unified_diff_added_lines(diff: &str) -> BTreeMap<String, BTreeSet<u32>> {
    let mut out: BTreeMap<String, BTreeSet<u32>> = BTreeMap::new();
    let mut path: Option<String> = None;
    let (mut old_left, mut new_left, mut new_line) = (0u32, 0u32, 0u32);

    for line in diff.lines() {
        if old_left == 0 && new_left == 0 {
            if line.starts_with("diff --git ") {
                path = None;
            } else if let Some(p) = line.strip_prefix("+++ ") {
                path = clean_path(p);
            } else if line.starts_with("@@ ") {
                if let Some(((_, oc), (ns, nc))) = parse_hunk_header(line) {
                    old_left = oc;
                    new_left = nc;
                    new_line = ns;
                    if let Some(p) = &path {
                        out.entry(p.clone()).or_default();
                    }
                }
            }
            continue;
        }
        if line.starts_with('\\') {
            continue; // "\ No newline at end of file"
        }
        if line.starts_with('+') && new_left > 0 {
            if let Some(p) = &path {
                out.entry(p.clone()).or_default().insert(new_line);
            }
            new_line += 1;
            new_left -= 1;
        } else if line.starts_with('-') && old_left > 0 {
            old_left -= 1;
        } else if line.starts_with(' ') {
            // context line (not produced with --unified=0, tolerated)
            old_left = old_left.saturating_sub(1);
            if new_left > 0 {
                new_left -= 1;
                new_line += 1;
            }
        }
    }
    out.retain(|_, v| !v.is_empty());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(m: &BTreeMap<String, BTreeSet<u32>>, p: &str) -> Vec<u32> {
        m.get(p).map(|s| s.iter().copied().collect()).unwrap_or_default()
    }

    #[test]
    fn lab_commit_adds_lines_4_to_9() {
        let d = "37aab8163efa1d88549e86c01a3116c04769f2e1\ndiff --git a/notes.txt b/notes.txt\nindex 1..2 100644\n--- a/notes.txt\n+++ b/notes.txt\n@@ -3,0 +4,6 @@ human line 3\n+codex line 1\n+codex line 2\n+codex line 3\n+codex line 4\n+my line 1\n+my line 2\n";
        let m = parse_unified_diff_added_lines(d);
        assert_eq!(lines(&m, "notes.txt"), vec![4, 5, 6, 7, 8, 9]);
    }

    #[test]
    fn new_file_and_deletions_and_single_line_hunks() {
        let d = "diff --git a/new.rs b/new.rs\nnew file mode 100644\n--- /dev/null\n+++ b/new.rs\n@@ -0,0 +1,2 @@\n+a\n+b\ndiff --git a/old.rs b/old.rs\n--- a/old.rs\n+++ b/old.rs\n@@ -5 +5 @@\n-x\n+y\n@@ -10,2 +10,0 @@\n-gone1\n-gone2\n";
        let m = parse_unified_diff_added_lines(d);
        assert_eq!(lines(&m, "new.rs"), vec![1, 2]);
        assert_eq!(lines(&m, "old.rs"), vec![5]);
    }

    #[test]
    fn plus_plus_plus_content_is_not_a_header() {
        // an added line whose text starts with "++ " must be counted as content
        let d = "diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n@@ -0,0 +1,1 @@\n+++ not a header\n";
        let m = parse_unified_diff_added_lines(d);
        assert_eq!(lines(&m, "a.txt"), vec![1]);
    }

    #[test]
    fn deleted_file_and_no_newline_marker() {
        let d = "diff --git a/d.txt b/d.txt\ndeleted file mode 100644\n--- a/d.txt\n+++ /dev/null\n@@ -1,2 +0,0 @@\n-a\n-b\ndiff --git a/e.txt b/e.txt\n--- a/e.txt\n+++ b/e.txt\n@@ -1 +1 @@\n-old\n\\ No newline at end of file\n+new\n\\ No newline at end of file\n";
        let m = parse_unified_diff_added_lines(d);
        assert!(!m.contains_key("d.txt"));
        assert_eq!(lines(&m, "e.txt"), vec![1]);
    }
}
