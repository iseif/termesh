use std::fs;
use std::path::Path;
use std::process::Command;

use termesh_core::GitOperation;
use termesh_git::{GitService, RealGitService};

fn git(root: &Path, args: &[&str]) {
    let output = Command::new("git").current_dir(root).args(args).output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
}

fn git_stdout(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git").current_dir(root).args(args).output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn commit_consumes_only_the_explicit_index() {
    if Command::new("git").arg("--version").output().is_err() {
        return;
    }
    let stamp =
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let root = std::env::temp_dir().join(format!("termesh-git-{stamp}"));
    fs::create_dir_all(&root).unwrap();
    git(&root, &["init", "-q"]);
    git(&root, &["config", "user.name", "Termesh Test"]);
    git(&root, &["config", "user.email", "termesh@example.invalid"]);
    fs::write(root.join("staged.txt"), "base\n").unwrap();
    fs::write(root.join("unstaged.txt"), "base\n").unwrap();
    git(&root, &["add", "--", "staged.txt", "unstaged.txt"]);
    git(&root, &["commit", "-qm", "initial"]);
    fs::write(root.join("staged.txt"), "staged change\n").unwrap();
    fs::write(root.join("unstaged.txt"), "must survive\n").unwrap();
    git(&root, &["add", "--", "staged.txt"]);

    let mut service = RealGitService::new();
    let before = service.snapshot(&root).unwrap();
    assert!(before.files.iter().any(|file| file.index.is_some()));
    assert!(before.files.iter().any(|file| file.worktree.is_some()));

    service.execute(&root, &GitOperation::Commit { message: "staged only".into() }).unwrap();

    let after = service.snapshot(&root).unwrap();
    assert!(after.files.iter().all(|file| file.index.is_none()));
    assert!(after.files.iter().any(|file| file.worktree.is_some()));
    assert_eq!(fs::read_to_string(root.join("unstaged.txt")).unwrap(), "must survive\n");
    let _ = fs::remove_dir_all(root);
}

#[test]
fn push_without_upstream_publishes_branch_to_origin() {
    if Command::new("git").arg("--version").output().is_err() {
        return;
    }
    let stamp =
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let base = std::env::temp_dir().join(format!("termesh-git-push-{stamp}"));
    let root = base.join("working");
    let remote = base.join("origin.git");
    fs::create_dir_all(&root).unwrap();
    fs::create_dir_all(&remote).unwrap();
    git(&remote, &["init", "--bare", "-q"]);
    git(&root, &["init", "-q", "-b", "main"]);
    git(&root, &["config", "user.name", "Termesh Test"]);
    git(&root, &["config", "user.email", "termesh@example.invalid"]);
    fs::write(root.join("published.txt"), "published\n").unwrap();
    git(&root, &["add", "--", "published.txt"]);
    git(&root, &["commit", "-qm", "publish me"]);
    git(&root, &["switch", "-qc", "feature/first-push"]);
    git(&root, &["remote", "add", "origin", remote.to_str().unwrap()]);

    let mut service = RealGitService::new();
    service.execute(&root, &GitOperation::Push).unwrap();

    let upstream =
        git_stdout(&root, &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{upstream}"]);
    assert_eq!(upstream.trim(), "origin/feature/first-push");
    git(&remote, &["rev-parse", "--verify", "refs/heads/feature/first-push"]);
    let _ = fs::remove_dir_all(base);
}
