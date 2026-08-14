use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use termesh_core::{
    GitBranch, GitDiffTarget, GitFailure, GitFailureKind, GitFileDiff, GitOperation,
    GitRepositorySnapshot, GitResult,
};

use crate::{bounded_context_diff, bounded_diff, parse_status, GitService};

const DIFF_LIMIT: usize = 256 * 1024;
const ERROR_LIMIT: usize = 16 * 1024;

#[derive(Debug, Default)]
pub struct RealGitService;

impl RealGitService {
    pub fn new() -> Self {
        Self
    }

    fn roots(&self, workspace: &Path) -> GitResult<(PathBuf, PathBuf, PathBuf)> {
        let root = run_checked(workspace, &["rev-parse", "--show-toplevel"])?;
        let repository_root = PathBuf::from(text(&root.stdout).trim_end());
        if repository_root.as_os_str().is_empty() {
            return Err(failure(
                GitFailureKind::InvalidOutput,
                "Git reported an empty repository root",
            ));
        }
        let prefix = run_checked(workspace, &["rev-parse", "--show-prefix"])?;
        let prefix = PathBuf::from(text(&prefix.stdout).trim_end());
        let workspace_root = if prefix.as_os_str().is_empty() {
            repository_root.clone()
        } else {
            repository_root.join(&prefix)
        };
        let scope = if prefix.as_os_str().is_empty() { ".".into() } else { prefix };
        Ok((repository_root, workspace_root, scope))
    }
}

impl GitService for RealGitService {
    fn snapshot(&mut self, workspace: &Path) -> GitResult<GitRepositorySnapshot> {
        let (repository_root, workspace_root, scope) = self.roots(workspace)?;
        // `--no-optional-locks` keeps a passive refresh from taking `.git/index.lock`: we
        // refresh on every coalesced filesystem batch, and the developer may be running
        // `git add -p` or a rebase in the managed terminal at the same time (ADR-0010 §7).
        let status = run_checked(
            &repository_root,
            &["--no-optional-locks", "status", "--porcelain=v2", "--branch", "-z"],
        )?;
        let (branch, files) = parse_status(&repository_root, &status.stdout)?;
        let worktree_args = diff_args(false, scope.as_os_str());
        let index_args = diff_args(true, scope.as_os_str());
        let worktree = run_os(&repository_root, &worktree_args)?;
        let index = run_os(&repository_root, &index_args)?;
        Ok(GitRepositorySnapshot {
            repository_root,
            workspace_root,
            branch,
            files,
            context_diff: bounded_context_diff(&index.stdout, &worktree.stdout, DIFF_LIMIT)?,
        })
    }

    fn diff(
        &mut self,
        workspace: &Path,
        path: &Path,
        target: GitDiffTarget,
    ) -> GitResult<GitFileDiff> {
        let (repository_root, _, _) = self.roots(workspace)?;
        let args = diff_args(target == GitDiffTarget::Index, path.as_os_str());
        let output = run_os(&repository_root, &args)?;
        bounded_diff(path.to_path_buf(), target, &output.stdout, DIFF_LIMIT)
    }

    fn branches(&mut self, workspace: &Path) -> GitResult<Vec<GitBranch>> {
        let (repository_root, _, _) = self.roots(workspace)?;
        let output = run_checked(
            &repository_root,
            &["for-each-ref", "--format=%(HEAD)%00%(refname:short)", "refs/heads"],
        )?;
        let mut branches = Vec::new();
        for line in output.stdout.split(|byte| *byte == b'\n').filter(|line| !line.is_empty()) {
            let separator = line.iter().position(|byte| *byte == 0).ok_or_else(|| {
                failure(GitFailureKind::InvalidOutput, "malformed local branch record")
            })?;
            let name = std::str::from_utf8(&line[separator + 1..]).map_err(|_| {
                failure(GitFailureKind::InvalidOutput, "branch name is not valid UTF-8")
            })?;
            branches.push(GitBranch { name: name.into(), current: &line[..separator] == b"*" });
        }
        branches.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(branches)
    }

    fn execute(&mut self, workspace: &Path, operation: &GitOperation) -> GitResult<String> {
        if matches!(operation, GitOperation::Commit { message } if message.trim().is_empty()) {
            return Err(failure(GitFailureKind::Command, "commit message cannot be empty"));
        }
        if let GitOperation::Checkout { branch } = operation {
            if !self.branches(workspace)?.iter().any(|item| item.name == *branch) {
                return Err(failure(GitFailureKind::Command, "branch is not a local branch"));
            }
        }
        let (repository_root, _, _) = self.roots(workspace)?;
        let head_exists = run_raw(&repository_root, &["rev-parse", "--verify", "HEAD"])
            .is_ok_and(|output| output.status.success());
        let args = if matches!(operation, GitOperation::Push) {
            push_args_for_repository(&repository_root)?
        } else {
            operation_args(operation, head_exists)
        };
        let output = run_os(&repository_root, &args)?;
        let summary = text(&output.stdout).trim().to_owned();
        Ok(if summary.is_empty() { "Git operation completed".into() } else { summary })
    }
}

fn push_args_for_repository(repository_root: &Path) -> GitResult<Vec<OsString>> {
    let branch = run_raw(repository_root, &["symbolic-ref", "--quiet", "--short", "HEAD"])
        .map_err(|error| {
            failure(GitFailureKind::Command, &format!("could not inspect Git branch: {error}"))
        })?;
    if !branch.status.success() {
        return Err(failure(GitFailureKind::Command, "cannot publish a detached HEAD"));
    }
    let branch = std::str::from_utf8(&branch.stdout)
        .map_err(|_| failure(GitFailureKind::InvalidOutput, "Git branch is not valid UTF-8"))?
        .trim_end();
    if branch.is_empty() {
        return Err(failure(GitFailureKind::InvalidOutput, "Git reported an empty branch"));
    }

    let upstream = run_raw(
        repository_root,
        &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{upstream}"],
    )
    .map_err(|error| {
        failure(GitFailureKind::Command, &format!("could not inspect Git upstream: {error}"))
    })?;
    let has_upstream = upstream.status.success() && !text(&upstream.stdout).trim().is_empty();
    Ok(push_args(branch, has_upstream))
}

fn push_args(branch: &str, has_upstream: bool) -> Vec<OsString> {
    if has_upstream {
        vec!["push".into()]
    } else {
        vec!["push".into(), "--set-upstream".into(), "origin".into(), branch.into()]
    }
}

pub(crate) fn operation_args(operation: &GitOperation, head_exists: bool) -> Vec<OsString> {
    match operation {
        GitOperation::Stage { path } => vec!["add".into(), "--".into(), path.as_os_str().into()],
        GitOperation::Unstage { path } if head_exists => {
            vec!["restore".into(), "--staged".into(), "--".into(), path.as_os_str().into()]
        }
        GitOperation::Unstage { path } => vec![
            "rm".into(),
            "--cached".into(),
            "--ignore-unmatch".into(),
            "--".into(),
            path.as_os_str().into(),
        ],
        GitOperation::Commit { message } => vec!["commit".into(), "-m".into(), message.into()],
        GitOperation::Checkout { branch } => vec!["switch".into(), "--".into(), branch.into()],
        GitOperation::Fetch => vec!["fetch".into()],
        GitOperation::Pull => vec!["pull".into(), "--ff-only".into()],
        GitOperation::Push => vec!["push".into()],
    }
}

fn diff_args(index: bool, path: &OsStr) -> Vec<OsString> {
    // `diff` refreshes the index too, so it takes the same passive-read stance as `status`.
    let mut args: Vec<OsString> =
        ["--no-optional-locks", "diff", "--no-ext-diff", "--no-color", "--unified=3"]
            .into_iter()
            .map(Into::into)
            .collect();
    if index {
        args.push("--cached".into());
    }
    args.push("--".into());
    args.push(path.into());
    args
}

fn run_checked(cwd: &Path, args: &[&str]) -> GitResult<Output> {
    let values: Vec<OsString> = args.iter().map(OsString::from).collect();
    run_os(cwd, &values)
}

fn run_os(cwd: &Path, args: &[OsString]) -> GitResult<Output> {
    let output = Command::new("git")
        .current_dir(cwd)
        .env("GIT_TERMINAL_PROMPT", "0")
        .args(args)
        .output()
        .map_err(|error| match error.kind() {
            io::ErrorKind::NotFound => {
                failure(GitFailureKind::Unavailable, "Git executable not found")
            }
            _ => failure(GitFailureKind::Command, &format!("could not run Git: {error}")),
        })?;
    check_output(output)
}

fn run_raw(cwd: &Path, args: &[&str]) -> io::Result<Output> {
    Command::new("git").current_dir(cwd).env("GIT_TERMINAL_PROMPT", "0").args(args).output()
}

fn check_output(output: Output) -> GitResult<Output> {
    if output.status.success() {
        return Ok(output);
    }
    let message = bounded_error(&output.stderr);
    let kind = if message.to_ascii_lowercase().contains("not a git repository") {
        GitFailureKind::NotRepository
    } else {
        GitFailureKind::Command
    };
    Err(failure(kind, if message.is_empty() { "Git command failed" } else { &message }))
}

fn bounded_error(bytes: &[u8]) -> String {
    let text = text(bytes);
    let mut end = ERROR_LIMIT.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].trim().into()
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn failure(kind: GitFailureKind, message: &str) -> GitFailure {
    GitFailure { kind, message: message.into() }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use termesh_core::GitOperation;

    use super::{diff_args, operation_args, push_args};

    fn strings(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    #[test]
    fn mutations_use_safe_structured_arguments() {
        assert_eq!(
            operation_args(&GitOperation::Stage { path: "-odd name.rs".into() }, true),
            strings(&["add", "--", "-odd name.rs"])
        );
        assert_eq!(
            operation_args(&GitOperation::Unstage { path: "src/lib.rs".into() }, true),
            strings(&["restore", "--staged", "--", "src/lib.rs"])
        );
        assert_eq!(
            operation_args(&GitOperation::Commit { message: "fix parser".into() }, true),
            strings(&["commit", "-m", "fix parser"])
        );
        assert_eq!(
            operation_args(&GitOperation::Checkout { branch: "feature/x".into() }, true),
            strings(&["switch", "--", "feature/x"])
        );
        assert_eq!(operation_args(&GitOperation::Fetch, true), strings(&["fetch"]));
        assert_eq!(operation_args(&GitOperation::Pull, true), strings(&["pull", "--ff-only"]));
        assert_eq!(operation_args(&GitOperation::Push, true), strings(&["push"]));
        assert_eq!(push_args("feature/new", true), strings(&["push"]));
        assert_eq!(
            push_args("feature/new", false),
            strings(&["push", "--set-upstream", "origin", "feature/new"]),
        );
    }

    #[test]
    fn passive_reads_pass_no_optional_locks_before_the_subcommand() {
        // Git only accepts this as a global flag; after the subcommand it is a hard error,
        // so the position is part of the contract, not formatting.
        assert_eq!(
            diff_args(true, "src/lib.rs".as_ref()),
            strings(&[
                "--no-optional-locks",
                "diff",
                "--no-ext-diff",
                "--no-color",
                "--unified=3",
                "--cached",
                "--",
                "src/lib.rs",
            ])
        );
    }

    #[test]
    fn unstage_on_an_unborn_branch_removes_only_the_index_entry() {
        assert_eq!(
            operation_args(&GitOperation::Unstage { path: "new.rs".into() }, false),
            strings(&["rm", "--cached", "--ignore-unmatch", "--", "new.rs"])
        );
    }
}
