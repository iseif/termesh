use std::path::Path;

use termesh_core::{
    GitBranch, GitDiffTarget, GitFileDiff, GitOperation, GitRepositorySnapshot, GitResult,
};

/// Blocking Git boundary. Production calls it only from `GitWorker` (ADR-0010 §1).
pub trait GitService: Send + 'static {
    fn snapshot(&mut self, root: &Path) -> GitResult<GitRepositorySnapshot>;
    fn diff(&mut self, root: &Path, path: &Path, target: GitDiffTarget) -> GitResult<GitFileDiff>;
    fn branches(&mut self, root: &Path) -> GitResult<Vec<GitBranch>>;
    fn execute(&mut self, root: &Path, operation: &GitOperation) -> GitResult<String>;
}
