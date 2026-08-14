use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::ffi::OsString;

use termesh_core::{
    GitBranchStatus, GitChangeKind, GitFailure, GitFailureKind, GitFileStatus, GitResult,
};

/// Parse `git status --porcelain=v2 --branch -z` without flattening paths into UTF-8.
pub fn parse_status(root: &Path, bytes: &[u8]) -> GitResult<(GitBranchStatus, Vec<GitFileStatus>)> {
    let mut branch = GitBranchStatus::default();
    let mut files = Vec::new();
    let mut records = bytes.split(|byte| *byte == 0).filter(|record| !record.is_empty());

    while let Some(record) = records.next() {
        match record.first().copied() {
            Some(b'#') => parse_branch_header(root, record, &mut branch)?,
            Some(b'1') => files.push(parse_ordinary(root, record)?),
            Some(b'2') => {
                let original = records
                    .next()
                    .ok_or_else(|| invalid(root, "rename record is missing its original path"))?;
                files.push(parse_renamed(root, record, original)?);
            }
            Some(b'u') => files.push(parse_unmerged(root, record)?),
            Some(b'?') => {
                let path = record
                    .strip_prefix(b"? ")
                    .ok_or_else(|| invalid(root, "malformed untracked record"))?;
                files.push(GitFileStatus {
                    path: path_from_git_bytes(root, path)?,
                    index: None,
                    worktree: Some(GitChangeKind::Untracked),
                });
            }
            Some(b'!') => {}
            Some(kind) => {
                return Err(invalid(root, &format!("unknown porcelain-v2 record type {kind:?}")));
            }
            None => {}
        }
    }

    files.sort_by(|left, right| left.path.cmp(&right.path));
    Ok((branch, files))
}

fn parse_branch_header(root: &Path, record: &[u8], branch: &mut GitBranchStatus) -> GitResult<()> {
    let text = std::str::from_utf8(record)
        .map_err(|_| invalid(root, "branch header is not valid UTF-8"))?;
    if let Some(oid) = text.strip_prefix("# branch.oid ") {
        branch.oid = (oid != "(initial)").then(|| oid.to_owned());
    } else if let Some(head) = text.strip_prefix("# branch.head ") {
        if head == "(detached)" {
            branch.detached = true;
            branch.head = None;
        } else {
            branch.detached = false;
            branch.head = Some(head.to_owned());
        }
    } else if let Some(upstream) = text.strip_prefix("# branch.upstream ") {
        branch.upstream = Some(upstream.to_owned());
    } else if let Some(counts) = text.strip_prefix("# branch.ab ") {
        let mut parts = counts.split_whitespace();
        branch.ahead = parse_count(root, parts.next(), '+', "ahead")?;
        branch.behind = parse_count(root, parts.next(), '-', "behind")?;
        if parts.next().is_some() {
            return Err(invalid(root, "branch.ab has extra fields"));
        }
    }
    Ok(())
}

fn parse_count(root: &Path, value: Option<&str>, prefix: char, name: &str) -> GitResult<usize> {
    let value = value.ok_or_else(|| invalid(root, &format!("branch.ab is missing {name}")))?;
    let digits = value
        .strip_prefix(prefix)
        .ok_or_else(|| invalid(root, &format!("branch.ab has malformed {name}")))?;
    digits.parse().map_err(|_| invalid(root, &format!("branch.ab has invalid {name}")))
}

fn parse_ordinary(root: &Path, record: &[u8]) -> GitResult<GitFileStatus> {
    let fields = fields(root, record, 9, "ordinary")?;
    let xy = parse_xy(root, fields[1], "ordinary")?;
    Ok(GitFileStatus {
        path: path_from_git_bytes(root, fields[8])?,
        index: change(root, xy[0], None)?,
        worktree: change(root, xy[1], None)?,
    })
}

fn parse_renamed(root: &Path, record: &[u8], original: &[u8]) -> GitResult<GitFileStatus> {
    let fields = fields(root, record, 10, "rename")?;
    let xy = parse_xy(root, fields[1], "rename")?;
    let original = path_from_git_bytes(root, original)?;
    Ok(GitFileStatus {
        path: path_from_git_bytes(root, fields[9])?,
        index: change(root, xy[0], Some(&original))?,
        worktree: change(root, xy[1], Some(&original))?,
    })
}

fn parse_unmerged(root: &Path, record: &[u8]) -> GitResult<GitFileStatus> {
    let fields = fields(root, record, 11, "unmerged")?;
    let _ = parse_xy(root, fields[1], "unmerged")?;
    Ok(GitFileStatus {
        path: path_from_git_bytes(root, fields[10])?,
        index: Some(GitChangeKind::Conflicted),
        worktree: Some(GitChangeKind::Conflicted),
    })
}

fn fields<'a>(
    root: &Path,
    record: &'a [u8],
    expected: usize,
    kind: &str,
) -> GitResult<Vec<&'a [u8]>> {
    let values: Vec<&[u8]> = record.splitn(expected, |byte| *byte == b' ').collect();
    if values.len() != expected || values.last().is_some_and(|value| value.is_empty()) {
        return Err(invalid(root, &format!("malformed {kind} record")));
    }
    Ok(values)
}

fn parse_xy(root: &Path, value: &[u8], kind: &str) -> GitResult<[u8; 2]> {
    value.try_into().map_err(|_| invalid(root, &format!("malformed {kind} XY status")))
}

fn change(root: &Path, code: u8, original: Option<&PathBuf>) -> GitResult<Option<GitChangeKind>> {
    let kind = match code {
        b'.' | b' ' => return Ok(None),
        b'M' | b'T' => GitChangeKind::Modified,
        b'A' | b'C' => GitChangeKind::Added,
        b'D' => GitChangeKind::Deleted,
        b'R' => GitChangeKind::Renamed {
            from: original
                .cloned()
                .ok_or_else(|| invalid(root, "rename status is missing its original path"))?,
        },
        b'U' => GitChangeKind::Conflicted,
        other => {
            return Err(invalid(root, &format!("unknown porcelain-v2 status code {other:?}")));
        }
    };
    Ok(Some(kind))
}

#[cfg(unix)]
fn path_from_git_bytes(_root: &Path, value: &[u8]) -> GitResult<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    Ok(PathBuf::from(OsString::from_vec(value.to_vec())))
}

#[cfg(not(unix))]
fn path_from_git_bytes(root: &Path, value: &[u8]) -> GitResult<PathBuf> {
    let value = std::str::from_utf8(value)
        .map_err(|_| invalid(root, "Git emitted a path that is not valid UTF-8"))?;
    Ok(PathBuf::from(value))
}

fn invalid(root: &Path, message: &str) -> GitFailure {
    GitFailure {
        kind: GitFailureKind::InvalidOutput,
        message: format!("{}: {message}", root.display()),
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use termesh_core::{GitChangeKind, GitFailureKind};

    use super::parse_status;

    #[test]
    fn parses_branch_counts_dual_state_rename_untracked_and_conflict() {
        // Removing either XY column, forgetting the second rename path, or treating an
        // unmerged record as a normal modification must break these literal expectations.
        let input = b"# branch.oid abc123\0# branch.head feature/git\0# branch.upstream origin/feature/git\0# branch.ab +2 -1\0\
1 MM N... 100644 100644 100644 aaaaaaa bbbbbbb src/lib.rs\0\
2 R. N... 100644 100644 100644 aaaaaaa bbbbbbb R100 src/new.rs\0src/old.rs\0\
? notes with spaces.md\0\
u UU N... 100644 100644 100644 100644 aaaaaaa bbbbbbb ccccccc conflict.rs\0";

        let (branch, files) = parse_status(Path::new("/repo"), input).unwrap();

        assert_eq!(branch.oid.as_deref(), Some("abc123"));
        assert_eq!(branch.head.as_deref(), Some("feature/git"));
        assert_eq!(branch.upstream.as_deref(), Some("origin/feature/git"));
        assert_eq!((branch.ahead, branch.behind), (2, 1));
        assert_eq!(files.len(), 4);
        assert_eq!(files[0].path, PathBuf::from("conflict.rs"));
        assert_eq!(files[0].index, Some(GitChangeKind::Conflicted));
        assert_eq!(files[0].worktree, Some(GitChangeKind::Conflicted));
        assert_eq!(files[1].path, PathBuf::from("notes with spaces.md"));
        assert_eq!(files[1].worktree, Some(GitChangeKind::Untracked));
        assert_eq!(files[2].path, PathBuf::from("src/lib.rs"));
        assert_eq!(files[2].index, Some(GitChangeKind::Modified));
        assert_eq!(files[2].worktree, Some(GitChangeKind::Modified));
        assert_eq!(
            files[3].index,
            Some(GitChangeKind::Renamed { from: PathBuf::from("src/old.rs") })
        );
    }

    #[test]
    fn parses_detached_and_unborn_heads() {
        let detached = parse_status(
            Path::new("/repo"),
            b"# branch.oid abcdef123456\0# branch.head (detached)\0",
        )
        .unwrap()
        .0;
        assert!(detached.detached);
        assert_eq!(detached.head, None);
        assert_eq!(detached.oid.as_deref(), Some("abcdef123456"));

        let unborn =
            parse_status(Path::new("/repo"), b"# branch.oid (initial)\0# branch.head main\0")
                .unwrap()
                .0;
        assert!(!unborn.detached);
        assert_eq!(unborn.head.as_deref(), Some("main"));
        assert_eq!(unborn.oid, None);
    }

    #[test]
    fn malformed_records_fail_instead_of_disappearing() {
        let error = parse_status(Path::new("/repo"), b"1 MM too-short\0").unwrap_err();
        assert_eq!(error.kind, GitFailureKind::InvalidOutput);
        assert!(error.message.contains("ordinary"));
    }

    #[test]
    fn leading_dash_and_unicode_paths_remain_data() {
        let (_, files) =
            parse_status(Path::new("/repo"), "? -odd.rs\0? src/שלום.rs\0".as_bytes()).unwrap();
        assert_eq!(files[0].path, PathBuf::from("-odd.rs"));
        assert_eq!(files[1].path, PathBuf::from("src/שלום.rs"));
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_paths_keep_their_os_identity() {
        use std::os::unix::ffi::OsStrExt;

        let (_, files) = parse_status(Path::new("/repo"), b"? bad-\xff.rs\0").unwrap();
        assert_eq!(files[0].path.as_os_str().as_bytes(), b"bad-\xff.rs");
    }
}
