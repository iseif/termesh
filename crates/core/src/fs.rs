//! Filesystem data types shared across the service boundary.
//!
//! These live in `core` rather than in `filesystem` because [`crate::AppMessage`] has to
//! carry them: the worker thread produces them and the single state owner consumes them
//! (ARCHITECTURE.md §7.1, §7.2 — `core` is the shared-types crate). The
//! `FileSystemService` trait and its implementations stay in `filesystem`, which
//! re-exports everything here so call sites see one module.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::{BufferId, LocationRequestId, NodeId, PreviewRequestId};

/// What a directory entry is, determined *without* following symlinks — a symlink
/// reports as [`EntryKind::Symlink`] whatever it points at (ADR-0005 §6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum EntryKind {
    Dir,
    File,
    Symlink,
}

impl EntryKind {
    /// Directories sort before everything else in the explorer.
    pub fn sort_rank(self) -> u8 {
        match self {
            EntryKind::Dir => 0,
            EntryKind::File | EntryKind::Symlink => 1,
        }
    }
}

/// One entry in a directory listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntryInfo {
    /// The file name only. Kept as an `OsString` so non-UTF-8 names survive intact;
    /// rendering lossy-converts, but we never lose the real name (ADR-0005 §6).
    pub name: OsString,
    pub path: PathBuf,
    pub kind: EntryKind,
}

/// Filesystem failures, in the vocabulary the explorer actually needs.
///
/// Deliberately not `std::io::Error`: these get stored in tree nodes to render a failed
/// expansion inline, and `io::Error` is neither `Clone` nor `PartialEq`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FsError {
    NotFound(PathBuf),
    PermissionDenied(PathBuf),
    NotADirectory(PathBuf),
    AlreadyExists(PathBuf),
    Other { path: PathBuf, message: String },
}

impl FsError {
    /// The path the failure is about, for attaching the error to a tree node.
    pub fn path(&self) -> &Path {
        match self {
            FsError::NotFound(p)
            | FsError::PermissionDenied(p)
            | FsError::NotADirectory(p)
            | FsError::AlreadyExists(p)
            | FsError::Other { path: p, .. } => p,
        }
    }
}

impl std::fmt::Display for FsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FsError::NotFound(p) => write!(f, "not found: {}", p.display()),
            FsError::PermissionDenied(p) => write!(f, "permission denied: {}", p.display()),
            FsError::NotADirectory(p) => write!(f, "not a directory: {}", p.display()),
            FsError::AlreadyExists(p) => write!(f, "already exists: {}", p.display()),
            FsError::Other { path, message } => write!(f, "{}: {message}", path.display()),
        }
    }
}

impl std::error::Error for FsError {}

pub type FsResult<T> = Result<T, FsError>;

/// Work sent *to* the filesystem worker thread.
///
/// Intentionally *not* `#[non_exhaustive]`: this is internal vocabulary between our own
/// crates, and we want the compiler to flag every unhandled variant rather than letting
/// a wildcard arm swallow it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FsRequest {
    /// List one directory level for the tree node that asked for it.
    ReadDir {
        id: NodeId,
        path: PathBuf,
    },
    /// Read a whole file for a buffer that is being opened.
    ///
    /// Opening a file is blocking I/O like any other, so it goes through the worker
    /// rather than the render loop — a cold file on a network mount must not freeze the
    /// UI any more than a cold directory does (ADR-0005 §1).
    ReadFile {
        buffer: BufferId,
        path: PathBuf,
    },
    /// Read a small line window for a search-result preview without opening a buffer.
    ReadPreview {
        request: PreviewRequestId,
        path: PathBuf,
        line: usize,
        context: usize,
    },
    /// Canonicalize a diagnostic path before the model decides whether it is safe to
    /// open inside the current workspace.
    ResolvePath {
        request: LocationRequestId,
        path: PathBuf,
    },
    /// Write a buffer back to disk.
    ///
    /// `version` is the buffer revision these bytes were taken from; it comes back on
    /// [`FsEvent::FileSaved`] so the buffer only clears its dirty flag if nothing was
    /// typed while the write was in flight. Carried as a raw `u64` because `core` must
    /// not depend on `editor`.
    WriteFile {
        buffer: BufferId,
        path: PathBuf,
        contents: Vec<u8>,
        version: u64,
    },
    /// Begin watching a root for changes.
    Watch(PathBuf),
    CreateFile(PathBuf),
    CreateDir(PathBuf),
    Rename {
        from: PathBuf,
        to: PathBuf,
    },
    /// Delete a file, or a directory and everything under it. The caller must have
    /// confirmed with the user first — the worker does not second-guess it.
    Remove {
        path: PathBuf,
        recursive: bool,
    },
    /// Stop the worker. Sent on shutdown so the thread exits its loop cleanly.
    Shutdown,
}

/// Results sent *back* from the filesystem worker into the state loop.
/// Exhaustive for the same reason as [`FsRequest`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FsEvent {
    DirLoaded {
        id: NodeId,
        entries: Vec<DirEntryInfo>,
    },
    DirFailed {
        id: NodeId,
        error: FsError,
    },
    /// A watched directory changed on disk and should be re-read. Paths, not ids,
    /// because the watcher knows nothing about the tree's identity scheme.
    ///
    /// Successful mutations report themselves this way too, so there is exactly one
    /// path that brings disk state back into the tree whether or not a watcher is running.
    Changed(Vec<PathBuf>),
    /// A create/rename/delete failed. Success needs no event — it arrives as `Changed`.
    MutationFailed(FsError),

    /// A file was read for a buffer being opened. Bytes, not text: decoding is the
    /// editor's decision, and `core` stays out of it.
    FileLoaded {
        buffer: BufferId,
        path: PathBuf,
        contents: Vec<u8>,
    },
    /// A buffer was written to disk at revision `version`.
    FileSaved {
        buffer: BufferId,
        version: u64,
    },
    /// A read or write for a specific buffer failed. Distinct from [`Self::MutationFailed`]
    /// because it has a buffer to report against, not just a path.
    FileFailed {
        buffer: BufferId,
        error: FsError,
    },
    PreviewLoaded {
        request: PreviewRequestId,
        path: PathBuf,
        start_line: usize,
        text: String,
    },
    PreviewFailed {
        request: PreviewRequestId,
        path: PathBuf,
        error: FsError,
    },
    PathResolved {
        request: LocationRequestId,
        path: PathBuf,
    },
    PathResolveFailed {
        request: LocationRequestId,
        path: PathBuf,
        error: FsError,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dirs_outrank_files_and_symlinks() {
        assert!(EntryKind::Dir.sort_rank() < EntryKind::File.sort_rank());
        assert_eq!(EntryKind::File.sort_rank(), EntryKind::Symlink.sort_rank());
    }

    #[test]
    fn error_carries_the_offending_path() {
        let e = FsError::PermissionDenied(PathBuf::from("/root/secret"));
        assert_eq!(e.path(), Path::new("/root/secret"));
        assert!(e.to_string().contains("permission denied"));
    }
}
