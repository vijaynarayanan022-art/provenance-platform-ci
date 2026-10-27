use sha2::{Digest, Sha256};
use std::fs;
use std::path::Path;

const MAX_DP_CELLS: usize = 4_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineDisposition {
    Unchanged,
    Replaced,
    Inserted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlignmentError {
    TooLarge,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlobError {
    Missing,
    HashMismatch,
}

/// Read a checkpoint blob and verify the file name against the SHA-256 of its bytes.
pub fn read_verified_blob(blobs_dir: &Path, blob_sha: &str) -> Result<Vec<u8>, BlobError> {
    if blob_sha.len() != 64 || !blob_sha.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(BlobError::Missing);
    }
    let content = fs::read(blobs_dir.join(blob_sha)).map_err(|_| BlobError::Missing)?;
    let actual = format!("{:x}", Sha256::digest(&content));
    if !actual.eq_ignore_ascii_case(blob_sha) {
        return Err(BlobError::HashMismatch);
    }
    Ok(content)
}

fn split_lines(text: &str) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    let normalized = text.replace("\r\n", "\n");
    let mut lines: Vec<_> = normalized.split('\n').map(str::to_owned).collect();
    if normalized.ends_with('\n') {
        lines.pop();
    }
    lines
}

/// Align blob lines to committed-file lines. Results are indexed by the committed file's
/// zero-based line number. CRLF and LF endings are normalized; line contents otherwise match
/// exactly. Unmatched lines in a changed region are paired in order as replacements, with any
/// excess committed-file lines treated as insertions.
pub fn align_lines(blob_text: &str, committed_text: &str) -> Result<Vec<LineDisposition>, AlignmentError> {
    let old = split_lines(blob_text);
    let new = split_lines(committed_text);
    let mut result = vec![LineDisposition::Unchanged; new.len()];

    let mut prefix = 0usize;
    while prefix < old.len().min(new.len()) && old[prefix] == new[prefix] {
        prefix += 1;
    }
    let mut suffix = 0usize;
    while suffix < old.len() - prefix
        && suffix < new.len() - prefix
        && old[old.len() - suffix - 1] == new[new.len() - suffix - 1]
    {
        suffix += 1;
    }

    let old_mid = &old[prefix..old.len() - suffix];
    let new_mid = &new[prefix..new.len() - suffix];
    if old_mid.len().saturating_mul(new_mid.len()) > MAX_DP_CELLS {
        return Err(AlignmentError::TooLarge);
    }

    let width = new_mid.len() + 1;
    let mut dp = vec![0u32; (old_mid.len() + 1) * width];
    for i in (0..old_mid.len()).rev() {
        for j in (0..new_mid.len()).rev() {
            let index = i * width + j;
            dp[index] = if old_mid[i] == new_mid[j] {
                dp[(i + 1) * width + j + 1] + 1
            } else {
                dp[(i + 1) * width + j].max(dp[i * width + j + 1])
            };
        }
    }

    let mut matches = Vec::with_capacity(prefix + suffix + dp[0] as usize);
    matches.extend((0..prefix).map(|i| (i, i)));
    let (mut i, mut j) = (0usize, 0usize);
    while i < old_mid.len() && j < new_mid.len() {
        if old_mid[i] == new_mid[j] {
            matches.push((prefix + i, prefix + j));
            i += 1;
            j += 1;
        } else if dp[(i + 1) * width + j] >= dp[i * width + j + 1] {
            i += 1;
        } else {
            j += 1;
        }
    }
    matches.extend((0..suffix).map(|offset| {
        (old.len() - suffix + offset, new.len() - suffix + offset)
    }));

    let (mut old_cursor, mut new_cursor) = (0usize, 0usize);
    for (old_match, new_match) in matches
        .into_iter()
        .chain(std::iter::once((old.len(), new.len())))
    {
        let old_gap = old_match - old_cursor;
        let new_gap = new_match - new_cursor;
        for offset in 0..new_gap {
            result[new_cursor + offset] = if offset < old_gap {
                LineDisposition::Replaced
            } else {
                LineDisposition::Inserted
            };
        }
        old_cursor = old_match.saturating_add(1);
        new_cursor = new_match.saturating_add(1);
    }

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dispositions(blob: &str, file: &str) -> Vec<LineDisposition> {
        align_lines(blob, file).unwrap()
    }

    #[test]
    fn aligns_identical_lines() {
        assert_eq!(
            dispositions("a\nb\nc\n", "a\nb\nc\n"),
            vec![LineDisposition::Unchanged; 3]
        );
    }

    #[test]
    fn marks_pure_insertion() {
        assert_eq!(
            dispositions("a\nc\n", "a\nb\nc\n"),
            vec![LineDisposition::Unchanged, LineDisposition::Inserted, LineDisposition::Unchanged]
        );
    }

    #[test]
    fn marks_pure_deletion_without_changing_survivors() {
        assert_eq!(
            dispositions("a\nb\nc\n", "a\nc\n"),
            vec![LineDisposition::Unchanged; 2]
        );
    }

    #[test]
    fn pairs_replacement_lines_in_order() {
        assert_eq!(
            dispositions("a\nb\nc\n", "a\nx\nc\n"),
            vec![LineDisposition::Unchanged, LineDisposition::Replaced, LineDisposition::Unchanged]
        );
    }

    #[test]
    fn empty_blob_marks_every_file_line_inserted() {
        assert_eq!(
            dispositions("", "a\nb\n"),
            vec![LineDisposition::Inserted; 2]
        );
    }

    #[test]
    fn empty_file_has_no_line_dispositions() {
        assert!(dispositions("a\nb\n", "").is_empty());
    }

    #[test]
    fn crlf_and_lf_are_equal() {
        assert_eq!(
            dispositions("a\nb\n", "a\r\nb\r\n"),
            vec![LineDisposition::Unchanged; 2]
        );
    }
}
