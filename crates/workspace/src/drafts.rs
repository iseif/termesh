//! Crash-recovery drafts persisted through [`FileSystemService`] (ADR-0014 §5).

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use termesh_filesystem::{EntryKind, FileSystemService, FsError, FsResult};

const DRAFT_VERSION: u32 = 1;
pub const RETENTION: Duration = Duration::from_secs(60 * 60 * 24 * 14);

/// Unsaved buffer text mirrored outside the project. Recovery only offers this text;
/// applying it remains an explicit transaction in the application model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Draft {
    pub path: PathBuf,
    pub saved_at: SystemTime,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftDiagnostic {
    pub file: PathBuf,
    pub problem: String,
    pub fallback: String,
}

#[derive(Serialize, Deserialize)]
struct StoredDraft {
    version: u32,
    path: PathBuf,
    saved_at_unix_seconds: u64,
    text: String,
}

/// Stable, dependency-free FNV-1a over the absolute path. The readable suffix is only
/// for humans inspecting the directory; identity comes from the whole path hash.
pub fn draft_file_name(path: &Path) -> PathBuf {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in path.to_string_lossy().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    let basename = path
        .file_name()
        .unwrap_or(path.as_os_str())
        .to_string_lossy()
        .chars()
        .take(48)
        .map(|value| {
            if value.is_ascii_alphanumeric() || matches!(value, '.' | '-' | '_') {
                value
            } else {
                '_'
            }
        })
        .collect::<String>();
    PathBuf::from(format!("{hash:016x}-{basename}.toml"))
}

pub fn encode_draft(draft: &Draft, file: &Path) -> FsResult<Vec<u8>> {
    let saved_at_unix_seconds = draft
        .saved_at
        .duration_since(UNIX_EPOCH)
        .map_err(|error| FsError::Other { path: file.to_path_buf(), message: error.to_string() })?
        .as_secs();
    let stored = StoredDraft {
        version: DRAFT_VERSION,
        path: draft.path.clone(),
        saved_at_unix_seconds,
        text: draft.text.clone(),
    };
    toml::to_string_pretty(&stored)
        .map(String::into_bytes)
        .map_err(|error| FsError::Other { path: file.to_path_buf(), message: error.to_string() })
}

pub fn parse_draft(bytes: &[u8], file: &Path) -> FsResult<Draft> {
    let text = std::str::from_utf8(bytes).map_err(|error| FsError::Other {
        path: file.to_path_buf(),
        message: format!("draft is not valid UTF-8: {error}"),
    })?;
    let stored: StoredDraft = toml::from_str(text)
        .map_err(|error| FsError::Other { path: file.to_path_buf(), message: error.to_string() })?;
    if stored.version > DRAFT_VERSION {
        return Err(FsError::Other {
            path: file.to_path_buf(),
            message: format!(
                "draft version {} is newer than this build understands",
                stored.version
            ),
        });
    }
    let saved_at = UNIX_EPOCH
        .checked_add(Duration::from_secs(stored.saved_at_unix_seconds))
        .ok_or(FsError::Other {
            path: file.to_path_buf(),
            message: "draft timestamp is out of range".into(),
        })?;
    Ok(Draft { path: stored.path, saved_at, text: stored.text })
}

pub fn write_draft(
    fs: &dyn FileSystemService,
    drafts_dir: &Path,
    draft: &Draft,
) -> FsResult<PathBuf> {
    fs.create_dir(drafts_dir)?;
    let file = drafts_dir.join(draft_file_name(&draft.path));
    let bytes = encode_draft(draft, &file)?;
    fs.write_file(&file, &bytes)?;
    Ok(file)
}

/// Load valid drafts for one workspace. A corrupt sibling is diagnosed and skipped;
/// one bad recovery file must not hide the rest.
pub fn drafts_for(
    fs: &dyn FileSystemService,
    drafts_dir: &Path,
    workspace_root: &Path,
) -> FsResult<(Vec<Draft>, Vec<DraftDiagnostic>)> {
    let entries = match fs.read_dir(drafts_dir) {
        Ok(entries) => entries,
        Err(FsError::NotFound(_)) => return Ok((Vec::new(), Vec::new())),
        Err(error) => return Err(error),
    };
    let mut drafts = Vec::new();
    let mut diagnostics = Vec::new();
    for entry in entries {
        if entry.kind != EntryKind::File {
            continue;
        }
        let result = fs.read_file(&entry.path).and_then(|bytes| parse_draft(&bytes, &entry.path));
        match result {
            Ok(draft) if draft.path.starts_with(workspace_root) => drafts.push(draft),
            Ok(_) => {}
            Err(error) => diagnostics.push(DraftDiagnostic {
                file: entry.path,
                problem: error.to_string(),
                fallback: "skipping this draft".into(),
            }),
        }
    }
    drafts.sort_by(|left, right| left.path.cmp(&right.path));
    Ok((drafts, diagnostics))
}

pub fn reap_drafts(
    fs: &dyn FileSystemService,
    drafts_dir: &Path,
    now: SystemTime,
    retention: Duration,
) -> FsResult<usize> {
    let entries = match fs.read_dir(drafts_dir) {
        Ok(entries) => entries,
        Err(FsError::NotFound(_)) => return Ok(0),
        Err(error) => return Err(error),
    };
    let mut reaped = 0;
    for entry in entries {
        if entry.kind != EntryKind::File {
            continue;
        }
        let Ok(bytes) = fs.read_file(&entry.path) else { continue };
        let Ok(draft) = parse_draft(&bytes, &entry.path) else { continue };
        if now.duration_since(draft.saved_at).is_ok_and(|age| age >= retention) {
            fs.remove_file(&entry.path)?;
            reaped += 1;
        }
    }
    Ok(reaped)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::time::{Duration, UNIX_EPOCH};
    use termesh_test_support::FakeFileSystem;

    #[test]
    fn two_projects_with_the_same_relative_path_do_not_collide() {
        let a = draft_file_name(Path::new("/a/src/main.rs"));
        let b = draft_file_name(Path::new("/b/src/main.rs"));
        assert_ne!(a, b);
        assert!(a.to_string_lossy().contains("main.rs"));
        assert!(b.to_string_lossy().contains("main.rs"));
    }

    #[test]
    fn a_draft_round_trips_through_the_filesystem_service() {
        let fs = FakeFileSystem::new();
        let draft = Draft {
            path: "/proj/src/main.rs".into(),
            saved_at: UNIX_EPOCH + Duration::from_secs(1234),
            text: "// unsaved\n".into(),
        };

        write_draft(&fs, Path::new("/cfg/drafts"), &draft).unwrap();
        let (drafts, diagnostics) =
            drafts_for(&fs, Path::new("/cfg/drafts"), Path::new("/proj")).unwrap();

        assert!(diagnostics.is_empty());
        assert_eq!(drafts, [draft]);
    }

    #[test]
    fn drafts_older_than_the_retention_window_are_reaped() {
        let fs = FakeFileSystem::new();
        let now = UNIX_EPOCH + Duration::from_secs(60 * 60 * 24 * 60);
        let old = Draft {
            path: "/proj/old.rs".into(),
            saved_at: now - Duration::from_secs(60 * 60 * 24 * 30),
            text: "old".into(),
        };
        let current =
            Draft { path: "/proj/current.rs".into(), saved_at: now, text: "current".into() };
        write_draft(&fs, Path::new("/cfg/drafts"), &old).unwrap();
        write_draft(&fs, Path::new("/cfg/drafts"), &current).unwrap();

        assert_eq!(reap_drafts(&fs, Path::new("/cfg/drafts"), now, RETENTION).unwrap(), 1);
        let (drafts, _) = drafts_for(&fs, Path::new("/cfg/drafts"), Path::new("/proj")).unwrap();
        assert_eq!(drafts, [current]);
    }
}
