use std::path::PathBuf;

use termesh_core::{
    GitBranch, GitChangeKind, GitDiffTarget, GitFailure, GitFileDiff, GitOperation,
    GitRepositorySnapshot, GitRequestId,
};
use termesh_ui::Pane;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum GitGroup {
    Conflicts,
    Staged,
    Changes,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitStatusRow {
    pub group: GitGroup,
    pub path: PathBuf,
    pub target: GitDiffTarget,
    pub kind: GitChangeKind,
    pub outside_workspace: bool,
}

pub struct GitStatusOverlay {
    rows: Vec<GitStatusRow>,
    pub selected: usize,
    pub previous_focus: Pane,
}

impl GitStatusOverlay {
    pub fn new(snapshot: &GitRepositorySnapshot, previous_focus: Pane) -> Self {
        let workspace_prefix = snapshot.workspace_root.strip_prefix(&snapshot.repository_root).ok();
        let outside_workspace = |path: &std::path::Path| {
            workspace_prefix
                .is_none_or(|prefix| !prefix.as_os_str().is_empty() && !path.starts_with(prefix))
        };
        let mut rows = Vec::new();
        for file in &snapshot.files {
            let conflicted = matches!(file.index, Some(GitChangeKind::Conflicted))
                || matches!(file.worktree, Some(GitChangeKind::Conflicted));
            if conflicted {
                rows.push(GitStatusRow {
                    group: GitGroup::Conflicts,
                    path: file.path.clone(),
                    target: GitDiffTarget::Worktree,
                    kind: GitChangeKind::Conflicted,
                    outside_workspace: outside_workspace(&file.path),
                });
                continue;
            }
            if let Some(kind) = &file.index {
                rows.push(GitStatusRow {
                    group: GitGroup::Staged,
                    path: file.path.clone(),
                    target: GitDiffTarget::Index,
                    kind: kind.clone(),
                    outside_workspace: outside_workspace(&file.path),
                });
            }
            if let Some(kind) = &file.worktree {
                rows.push(GitStatusRow {
                    group: GitGroup::Changes,
                    path: file.path.clone(),
                    target: GitDiffTarget::Worktree,
                    kind: kind.clone(),
                    outside_workspace: outside_workspace(&file.path),
                });
            }
        }
        rows.sort_by(|left, right| {
            left.group.cmp(&right.group).then_with(|| left.path.cmp(&right.path))
        });
        Self { rows, selected: 0, previous_focus }
    }

    pub fn rows(&self) -> &[GitStatusRow] {
        &self.rows
    }

    pub fn selected(&self) -> Option<&GitStatusRow> {
        self.rows.get(self.selected)
    }

    pub fn move_up(&mut self) {
        if !self.rows.is_empty() {
            self.selected = (self.selected + self.rows.len() - 1) % self.rows.len();
        }
    }

    pub fn move_down(&mut self) {
        if !self.rows.is_empty() {
            self.selected = (self.selected + 1) % self.rows.len();
        }
    }
}

pub struct GitDiffOverlay {
    pub path: PathBuf,
    pub target: GitDiffTarget,
    pub text: Option<String>,
    pub truncated: bool,
    pub error: Option<String>,
    /// Why there is nothing to diff, when that is a fact about the path rather than a
    /// failure — an untracked file has no recorded version to compare against.
    pub notice: Option<String>,
    pub scroll: usize,
}

impl GitDiffOverlay {
    pub fn loading(path: PathBuf, target: GitDiffTarget) -> Self {
        Self { path, target, text: None, truncated: false, error: None, notice: None, scroll: 0 }
    }

    pub fn notice(path: PathBuf, target: GitDiffTarget, notice: String) -> Self {
        Self { notice: Some(notice), ..Self::loading(path, target) }
    }

    pub fn scroll_up(&mut self, amount: usize) {
        self.scroll = self.scroll.saturating_sub(amount);
    }

    pub fn scroll_down(&mut self, amount: usize) {
        let last_line = self.text.as_deref().map(str::lines).map(Iterator::count).unwrap_or(0);
        self.scroll = self.scroll.saturating_add(amount).min(last_line.saturating_sub(1));
    }
}

pub struct GitBranchesOverlay {
    pub branches: Vec<GitBranch>,
    pub selected: usize,
    pub previous_focus: Pane,
}

impl GitBranchesOverlay {
    pub fn new(branches: Vec<GitBranch>, previous_focus: Pane) -> Self {
        let selected = branches.iter().position(|branch| branch.current).unwrap_or(0);
        Self { branches, selected, previous_focus }
    }

    pub fn selected(&self) -> Option<&GitBranch> {
        self.branches.get(self.selected)
    }

    pub fn move_up(&mut self) {
        if !self.branches.is_empty() {
            self.selected = (self.selected + self.branches.len() - 1) % self.branches.len();
        }
    }

    pub fn move_down(&mut self) {
        if !self.branches.is_empty() {
            self.selected = (self.selected + 1) % self.branches.len();
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitLoadState {
    Idle,
    Loading,
    Ready,
    Unavailable(GitFailure),
    Stale(GitFailure),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitState {
    pub load: GitLoadState,
    pub snapshot: Option<GitRepositorySnapshot>,
    pub diff: Option<GitFileDiff>,
    pub branches: Vec<GitBranch>,
    pub active_refresh: Option<GitRequestId>,
    pub active_diff: Option<GitRequestId>,
    pub active_branches: Option<GitRequestId>,
    pub active_operation: Option<(GitRequestId, GitOperation)>,
    pub pending_refresh: bool,
}

impl Default for GitState {
    fn default() -> Self {
        Self {
            load: GitLoadState::Idle,
            snapshot: None,
            diff: None,
            branches: Vec::new(),
            active_refresh: None,
            active_diff: None,
            active_branches: None,
            active_operation: None,
            pending_refresh: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use termesh_core::{
        GitBranchStatus, GitChangeKind, GitContextDiff, GitDiffTarget, GitFileStatus,
        GitRepositorySnapshot,
    };
    use termesh_ui::Pane;

    use super::{GitGroup, GitStatusOverlay};

    fn snapshot() -> GitRepositorySnapshot {
        GitRepositorySnapshot {
            repository_root: "/repo".into(),
            workspace_root: "/repo".into(),
            branch: GitBranchStatus::default(),
            files: vec![
                GitFileStatus {
                    path: PathBuf::from("worktree.rs"),
                    index: None,
                    worktree: Some(GitChangeKind::Modified),
                },
                GitFileStatus {
                    path: PathBuf::from("staged.rs"),
                    index: Some(GitChangeKind::Modified),
                    worktree: None,
                },
                GitFileStatus {
                    path: PathBuf::from("both.rs"),
                    index: Some(GitChangeKind::Modified),
                    worktree: Some(GitChangeKind::Modified),
                },
                GitFileStatus {
                    path: PathBuf::from("conflict.rs"),
                    index: Some(GitChangeKind::Conflicted),
                    worktree: Some(GitChangeKind::Conflicted),
                },
            ],
            context_diff: GitContextDiff::default(),
        }
    }

    #[test]
    fn status_rows_group_conflicts_then_staged_then_worktree_changes() {
        let overlay = GitStatusOverlay::new(&snapshot(), Pane::Editor);
        assert_eq!(
            overlay
                .rows()
                .iter()
                .map(|row| (row.group, row.path.as_path(), row.target))
                .collect::<Vec<_>>(),
            vec![
                (GitGroup::Conflicts, Path::new("conflict.rs"), GitDiffTarget::Worktree),
                (GitGroup::Staged, Path::new("both.rs"), GitDiffTarget::Index),
                (GitGroup::Staged, Path::new("staged.rs"), GitDiffTarget::Index),
                (GitGroup::Changes, Path::new("both.rs"), GitDiffTarget::Worktree),
                (GitGroup::Changes, Path::new("worktree.rs"), GitDiffTarget::Worktree),
            ]
        );
    }
}
