use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use termesh_core::{
    GitBranch, GitDiffTarget, GitFailure, GitFailureKind, GitFileDiff, GitOperation,
    GitRepositorySnapshot, GitResult,
};
use termesh_git::GitService;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FakeGitCall {
    Snapshot { root: PathBuf },
    Diff { root: PathBuf, path: PathBuf, target: GitDiffTarget },
    Branches { root: PathBuf },
    Execute { root: PathBuf, operation: GitOperation },
}

#[derive(Clone)]
pub struct FakeGitControl {
    calls: Arc<Mutex<Vec<FakeGitCall>>>,
}

impl FakeGitControl {
    pub fn calls(&self) -> Vec<FakeGitCall> {
        self.calls.lock().expect("fake Git call log poisoned").clone()
    }
}

#[derive(Default)]
pub struct FakeGitService {
    snapshots: VecDeque<GitResult<GitRepositorySnapshot>>,
    diffs: VecDeque<GitResult<GitFileDiff>>,
    branches: VecDeque<GitResult<Vec<GitBranch>>>,
    executions: VecDeque<GitResult<String>>,
    calls: Arc<Mutex<Vec<FakeGitCall>>>,
}

impl FakeGitService {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn control(&self) -> FakeGitControl {
        FakeGitControl { calls: self.calls.clone() }
    }

    pub fn with_snapshot_result(mut self, result: GitResult<GitRepositorySnapshot>) -> Self {
        self.snapshots.push_back(result);
        self
    }

    pub fn with_diff_result(mut self, result: GitResult<GitFileDiff>) -> Self {
        self.diffs.push_back(result);
        self
    }

    pub fn with_branches_result(mut self, result: GitResult<Vec<GitBranch>>) -> Self {
        self.branches.push_back(result);
        self
    }

    pub fn with_execute_result(mut self, result: GitResult<String>) -> Self {
        self.executions.push_back(result);
        self
    }

    fn record(&self, call: FakeGitCall) {
        self.calls.lock().expect("fake Git call log poisoned").push(call);
    }
}

impl GitService for FakeGitService {
    fn snapshot(&mut self, root: &Path) -> GitResult<GitRepositorySnapshot> {
        self.record(FakeGitCall::Snapshot { root: root.to_path_buf() });
        self.snapshots.pop_front().unwrap_or_else(|| Err(missing_result("snapshot")))
    }

    fn diff(&mut self, root: &Path, path: &Path, target: GitDiffTarget) -> GitResult<GitFileDiff> {
        self.record(FakeGitCall::Diff {
            root: root.to_path_buf(),
            path: path.to_path_buf(),
            target,
        });
        self.diffs.pop_front().unwrap_or_else(|| Err(missing_result("diff")))
    }

    fn branches(&mut self, root: &Path) -> GitResult<Vec<GitBranch>> {
        self.record(FakeGitCall::Branches { root: root.to_path_buf() });
        self.branches.pop_front().unwrap_or_else(|| Err(missing_result("branches")))
    }

    fn execute(&mut self, root: &Path, operation: &GitOperation) -> GitResult<String> {
        self.record(FakeGitCall::Execute {
            root: root.to_path_buf(),
            operation: operation.clone(),
        });
        self.executions.pop_front().unwrap_or_else(|| Err(missing_result("execute")))
    }
}

fn missing_result(method: &str) -> GitFailure {
    GitFailure {
        kind: GitFailureKind::Command,
        message: format!("no scripted Git {method} result"),
    }
}

#[cfg(test)]
mod tests {
    use termesh_core::{
        GitBranch, GitBranchStatus, GitContextDiff, GitDiffTarget, GitOperation,
        GitRepositorySnapshot,
    };
    use termesh_git::GitService;

    use super::{FakeGitCall, FakeGitService};

    fn snapshot() -> GitRepositorySnapshot {
        GitRepositorySnapshot {
            repository_root: "/repo".into(),
            workspace_root: "/repo/workspace".into(),
            branch: GitBranchStatus::default(),
            files: Vec::new(),
            context_diff: GitContextDiff::default(),
        }
    }

    #[test]
    fn queued_results_are_returned_and_full_calls_are_recorded_in_order() {
        let mut service = FakeGitService::new()
            .with_snapshot_result(Ok(snapshot()))
            .with_branches_result(Ok(vec![GitBranch { name: "main".into(), current: true }]))
            .with_execute_result(Ok("fetched".into()));
        let control = service.control();

        service.snapshot("/repo/workspace".as_ref()).unwrap();
        service.branches("/repo/workspace".as_ref()).unwrap();
        service.execute("/repo/workspace".as_ref(), &GitOperation::Fetch).unwrap();

        assert_eq!(
            control.calls(),
            vec![
                FakeGitCall::Snapshot { root: "/repo/workspace".into() },
                FakeGitCall::Branches { root: "/repo/workspace".into() },
                FakeGitCall::Execute {
                    root: "/repo/workspace".into(),
                    operation: GitOperation::Fetch,
                },
            ]
        );
    }

    #[test]
    fn records_diff_paths_and_targets() {
        let mut service = FakeGitService::new().with_diff_result(Err(termesh_core::GitFailure {
            kind: termesh_core::GitFailureKind::Command,
            message: "scripted".into(),
        }));
        let control = service.control();
        let _ = service.diff("/repo".as_ref(), "src/lib.rs".as_ref(), GitDiffTarget::Index);
        assert_eq!(
            control.calls(),
            vec![FakeGitCall::Diff {
                root: "/repo".into(),
                path: "src/lib.rs".into(),
                target: GitDiffTarget::Index,
            }]
        );
    }
}
