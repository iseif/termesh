use std::path::PathBuf;

use termesh_core::{GitContextDiff, GitDiffTarget, GitFileDiff, GitResult};

pub fn bounded_diff(
    path: PathBuf,
    target: GitDiffTarget,
    bytes: &[u8],
    limit: usize,
) -> GitResult<GitFileDiff> {
    let (text, truncated) = bounded_text(bytes, limit);
    Ok(GitFileDiff { path, target, text, truncated })
}

pub fn bounded_context_diff(
    index: &[u8],
    worktree: &[u8],
    limit: usize,
) -> GitResult<GitContextDiff> {
    let (index, index_truncated) = bounded_text(index, limit);
    let (worktree, worktree_truncated) = bounded_text(worktree, limit);
    Ok(GitContextDiff { index, worktree, index_truncated, worktree_truncated })
}

fn bounded_text(bytes: &[u8], limit: usize) -> (String, bool) {
    let text = String::from_utf8_lossy(bytes);
    let truncated = bytes.len() > limit || text.len() > limit;
    if !truncated {
        return (text.into_owned(), false);
    }

    let mut end = limit.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_owned(), true)
}

#[cfg(test)]
mod tests {
    use termesh_core::GitDiffTarget;

    use super::{bounded_context_diff, bounded_diff};

    #[test]
    fn bounded_diff_keeps_complete_utf8_and_marks_truncation() {
        // Cutting directly at byte five would split the final beta. This catches a
        // byte-slice implementation that can create invalid Rust strings.
        let diff =
            bounded_diff("src/lib.rs".into(), GitDiffTarget::Worktree, "+αβγ\n".as_bytes(), 5)
                .unwrap();
        assert!(diff.truncated);
        assert!(diff.text.is_char_boundary(diff.text.len()));
        assert_eq!(diff.text, "+αβ");
    }

    #[test]
    fn context_diff_bounds_each_side_independently() {
        let diff = bounded_context_diff(b"staged-contents", b"worktree-contents", 7).unwrap();
        assert!(diff.index_truncated);
        assert!(diff.worktree_truncated);
        assert_eq!(diff.index, "staged-");
        assert_eq!(diff.worktree, "worktre");
    }

    #[test]
    fn invalid_process_bytes_are_replaced_not_rejected() {
        let diff =
            bounded_diff("binary-ish".into(), GitDiffTarget::Index, b"ok\xfftail", 64).unwrap();
        assert_eq!(diff.text, "ok�tail");
        assert!(!diff.truncated);
    }
}
