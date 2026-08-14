//! The `FileSystemService` boundary: the only door to the real filesystem.
//!
//! Per ADR-0005 §3 these methods are **blocking and synchronous**. They are called only
//! from the filesystem worker thread, so the non-blocking guarantee comes from *where*
//! they run, not from `async fn`. That keeps the trait object-safe, keeps the in-memory
//! fake trivial, and avoids colouring the codebase `async` before Phase 03 needs it.
//!
//! This shape is the template `GitService`, `PtyService`, and `LanguageService` follow.
//!
//! The data types themselves live in `core` because [`termesh_core::AppMessage`] has to
//! carry them across the worker/state boundary; they are re-exported here so callers
//! only ever need one import.

use std::path::{Path, PathBuf};

pub use termesh_core::{DirEntryInfo, EntryKind, FsError, FsResult};

/// Read and mutate the filesystem. Widgets and the agent reach the OS only through this
/// (CONTRIBUTING.md invariants, ARCHITECTURE.md §7.4) — never `std::fs` directly.
///
/// Every write method is a permission-gate chokepoint for the agent's future
/// `file.create` / `file.rename` tool calls, which is why they live behind one trait.
pub trait FileSystemService: Send + Sync {
    /// List one directory level. Does not recurse — the tree is lazy (ADR-0005 §2).
    ///
    /// **Contract:** entries come back sorted, directories first, then by name
    /// case-insensitively. Ordering is part of the contract rather than left to the
    /// caller so the real and fake implementations are interchangeable in tests.
    fn read_dir(&self, path: &Path) -> FsResult<Vec<DirEntryInfo>>;

    fn read_file(&self, path: &Path) -> FsResult<Vec<u8>>;

    /// Create an empty file. Errors with [`FsError::AlreadyExists`] rather than truncating.
    fn create_file(&self, path: &Path) -> FsResult<()>;

    /// Replace a file's contents, creating it if absent.
    ///
    /// Distinct from [`Self::create_file`] on purpose: creating is a user gesture that
    /// must not clobber, whereas writing is a deliberate overwrite. Phase 03's buffer
    /// save lands here too.
    fn write_file(&self, path: &Path, contents: &[u8]) -> FsResult<()>;

    /// Create a directory, including missing parents.
    fn create_dir(&self, path: &Path) -> FsResult<()>;

    fn rename(&self, from: &Path, to: &Path) -> FsResult<()>;

    fn remove_file(&self, path: &Path) -> FsResult<()>;

    /// Recursively delete a directory. Named for what it does: callers must confirm
    /// with the user (or hold an agent permission grant) before invoking it.
    fn remove_dir_all(&self, path: &Path) -> FsResult<()>;

    /// Resolve symlinks and `..` to an absolute path. Used as the loop guard when
    /// deciding whether a symlinked directory has already been visited.
    fn canonicalize(&self, path: &Path) -> FsResult<PathBuf>;
}

/// Apply the [`FileSystemService::read_dir`] ordering contract.
pub fn sort_entries(entries: &mut [DirEntryInfo]) {
    entries.sort_by(|a, b| {
        a.kind
            .sort_rank()
            .cmp(&b.kind.sort_rank())
            .then_with(|| {
                a.name
                    .to_string_lossy()
                    .to_lowercase()
                    .cmp(&b.name.to_string_lossy().to_lowercase())
            })
            // Tie-break on the raw name so equal-ignoring-case names stay deterministic.
            .then_with(|| a.name.cmp(&b.name))
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, kind: EntryKind) -> DirEntryInfo {
        DirEntryInfo { name: name.into(), path: PathBuf::from(name), kind }
    }

    #[test]
    fn dirs_sort_before_files_then_case_insensitively() {
        let mut v = vec![
            entry("README.md", EntryKind::File),
            entry("src", EntryKind::Dir),
            entry("Cargo.toml", EntryKind::File),
            entry("assets", EntryKind::Dir),
        ];
        sort_entries(&mut v);
        let names: Vec<_> = v.iter().map(|e| e.name.to_string_lossy().into_owned()).collect();
        assert_eq!(names, ["assets", "src", "Cargo.toml", "README.md"]);
    }

    #[test]
    fn sort_is_deterministic_for_names_differing_only_by_case() {
        let mut v = vec![entry("b", EntryKind::File), entry("B", EntryKind::File)];
        sort_entries(&mut v);
        let first = v[0].name.clone();
        sort_entries(&mut v);
        assert_eq!(v[0].name, first);
    }
}
