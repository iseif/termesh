//! Protocol-neutral Git state and worker messages (ADR-0010).
//!
//! CLI details stay in `termesh-git`; these types live here because the application
//! message bus and single-owner model must carry them without depending on a backend.

use std::path::PathBuf;

use crate::GitRequestId;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitChangeKind {
    Modified,
    Added,
    Deleted,
    Renamed { from: PathBuf },
    Untracked,
    Conflicted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitFileStatus {
    /// Repository-relative path. The repository root is carried by the snapshot.
    pub path: PathBuf,
    pub index: Option<GitChangeKind>,
    pub worktree: Option<GitChangeKind>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GitBranchStatus {
    pub oid: Option<String>,
    pub head: Option<String>,
    pub upstream: Option<String>,
    pub ahead: usize,
    pub behind: usize,
    pub detached: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitBranch {
    pub name: String,
    pub current: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GitContextDiff {
    pub index: String,
    pub worktree: String,
    pub index_truncated: bool,
    pub worktree_truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitRepositorySnapshot {
    pub repository_root: PathBuf,
    pub workspace_root: PathBuf,
    pub branch: GitBranchStatus,
    pub files: Vec<GitFileStatus>,
    pub context_diff: GitContextDiff,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitDiffTarget {
    Worktree,
    Index,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitFileDiff {
    pub path: PathBuf,
    pub target: GitDiffTarget,
    pub text: String,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitOperation {
    Stage { path: PathBuf },
    Unstage { path: PathBuf },
    Commit { message: String },
    Checkout { branch: String },
    Fetch,
    Pull,
    Push,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitRequest {
    Refresh { id: GitRequestId, root: PathBuf },
    Diff { id: GitRequestId, root: PathBuf, path: PathBuf, target: GitDiffTarget },
    Branches { id: GitRequestId, root: PathBuf },
    Execute { id: GitRequestId, root: PathBuf, operation: GitOperation },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitFailureKind {
    NotRepository,
    Unavailable,
    InvalidOutput,
    Command,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitFailure {
    pub kind: GitFailureKind,
    pub message: String,
}

pub type GitResult<T> = Result<T, GitFailure>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitEvent {
    Started {
        id: GitRequestId,
    },
    SnapshotLoaded {
        id: GitRequestId,
        snapshot: GitRepositorySnapshot,
    },
    DiffLoaded {
        id: GitRequestId,
        diff: GitFileDiff,
    },
    BranchesLoaded {
        id: GitRequestId,
        branches: Vec<GitBranch>,
    },
    OperationFinished {
        id: GitRequestId,
        operation: GitOperation,
        message: String,
        snapshot: GitRepositorySnapshot,
    },
    Failed {
        id: GitRequestId,
        operation_applied: bool,
        failure: GitFailure,
    },
}
