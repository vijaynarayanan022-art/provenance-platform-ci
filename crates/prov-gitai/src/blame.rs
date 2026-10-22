//! Parser for `git blame --line-porcelain` output.
//!
//! For each line in the file, extracts the introducing commit SHA and the
//! original line number in that commit's version. This is needed for PR range
//! analysis: we must know which commit introduced each added line and what line
//! number it had in that commit's version (to match against the per-commit
//! Git AI note).

use std::collections::HashMap;

/// One line's blame information.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlameLine {
    /// The SHA of the commit that introduced this line.
    pub commit: String,
    /// The line number in the original commit's version of the file (1-based).
    pub orig_line: u32,
    /// The line number in the final (HEAD) version of the file (1-based).
    pub final_line: u32,
}

/// Parse `git blame --line-porcelain <rev> -- <file>` output.
///
/// Returns a map from final line number -> BlameLine.
///
/// Line-porcelain format (per line):
/// ```text
/// <sha> <orig_line> <final_line> [<group_lines>]
/// author ...
/// ...
/// filename <path>
/// \t<content>
/// ```
pub fn parse_blame_porcelain(text: &str) -> HashMap<u32, BlameLine> {
    let mut result = HashMap::new();
    let mut current_commit: Option<String> = None;
    let mut current_orig: u32 = 0;
    let mut current_final: u32 = 0;

    for line in text.lines() {
        if line.starts_with('\t') {
            // This is the content line; we already captured the header.
            if let Some(ref sha) = current_commit {
                result.insert(
                    current_final,
                    BlameLine {
                        commit: sha.clone(),
                        orig_line: current_orig,
                        final_line: current_final,
                    },
                );
            }
            current_commit = None;
            continue;
        }

        // Check if this is a header line: <40-hex-sha> <orig> <final> [<group>]
        // Header lines start with a hex char and have at least 40 hex chars before the first space.
        if current_commit.is_none() && line.len() >= 40 {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 3 {
                let sha_candidate = parts[0];
                if sha_candidate.len() >= 40
                    && sha_candidate.chars().all(|c| c.is_ascii_hexdigit())
                {
                    if let (Ok(orig), Ok(fin)) = (parts[1].parse::<u32>(), parts[2].parse::<u32>())
                    {
                        current_commit = Some(sha_candidate.to_string());
                        current_orig = orig;
                        current_final = fin;
                    }
                }
            }
        }
        // Otherwise it's a metadata line (author, committer, etc.) — skip.
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_simple_blame() {
        let text = "\
abc1234567890123456789012345678901234abcd 1 1 1
author Test
author-mail <test@example.com>
author-time 1000000000
author-tz +0000
committer Test
committer-mail <test@example.com>
committer-time 1000000000
committer-tz +0000
summary first
filename notes.txt
\thello world
def1234567890123456789012345678901234abcd 3 2 1
author Test2
author-mail <test2@example.com>
author-time 1000000001
author-tz +0000
committer Test2
committer-mail <test2@example.com>
committer-time 1000000001
committer-tz +0000
summary second
filename notes.txt
\tline two
";
        let m = parse_blame_porcelain(text);
        assert_eq!(m.len(), 2);
        assert_eq!(m[&1].commit, "abc1234567890123456789012345678901234abcd");
        assert_eq!(m[&1].orig_line, 1);
        assert_eq!(m[&2].commit, "def1234567890123456789012345678901234abcd");
        assert_eq!(m[&2].orig_line, 3);
    }

    #[test]
    fn handles_group_headers() {
        // When a commit introduces multiple consecutive lines, only the first has 4 fields
        // (sha orig final group_count); subsequent lines in the group have 3 fields.
        let text = "\
aaa1234567890123456789012345678901234abcd 1 1 2
author A
filename f.txt
\tline1
aaa1234567890123456789012345678901234abcd 2 2
author A
filename f.txt
\tline2
";
        let m = parse_blame_porcelain(text);
        assert_eq!(m.len(), 2);
        assert_eq!(m[&1].orig_line, 1);
        assert_eq!(m[&2].orig_line, 2);
        assert_eq!(m[&1].commit, m[&2].commit);
    }

    #[test]
    fn handles_empty_input() {
        let m = parse_blame_porcelain("");
        assert!(m.is_empty());
    }

    #[test]
    fn handles_malformed_and_extra_whitespace() {
        let text = "not a sha\nauthor foo\n\tsome content\n";
        let m = parse_blame_porcelain(text);
        assert!(m.is_empty());
    }
}
