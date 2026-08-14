use std::sync::mpsc::{self, Sender};
use std::thread::JoinHandle;

use termesh_core::{GitEvent, GitRequest};

use crate::GitService;

enum WorkerMessage {
    Request(GitRequest),
    Shutdown,
}

/// Serializes all blocking Git work on one background thread.
pub struct GitWorker {
    tx: Sender<WorkerMessage>,
    handle: Option<JoinHandle<()>>,
}

impl GitWorker {
    pub fn spawn<S, F>(mut service: S, sink: F) -> Self
    where
        S: GitService,
        F: Fn(GitEvent) + Send + 'static,
    {
        let (tx, rx) = mpsc::channel();
        let handle = std::thread::Builder::new()
            .name("termesh-git".into())
            .spawn(move || {
                while let Ok(message) = rx.recv() {
                    let WorkerMessage::Request(request) = message else {
                        break;
                    };
                    run_request(&mut service, request, &sink);
                }
            })
            .expect("spawning the Git worker thread");
        Self { tx, handle: Some(handle) }
    }

    /// Queues a Git request. Returns false only after the worker has shut down.
    pub fn request(&self, request: GitRequest) -> bool {
        self.tx.send(WorkerMessage::Request(request)).is_ok()
    }
}

impl Drop for GitWorker {
    fn drop(&mut self) {
        let _ = self.tx.send(WorkerMessage::Shutdown);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn run_request<S, F>(service: &mut S, request: GitRequest, sink: &F)
where
    S: GitService,
    F: Fn(GitEvent),
{
    let id = match &request {
        GitRequest::Refresh { id, .. }
        | GitRequest::Diff { id, .. }
        | GitRequest::Branches { id, .. }
        | GitRequest::Execute { id, .. } => *id,
    };
    sink(GitEvent::Started { id });
    match request {
        GitRequest::Refresh { root, .. } => match service.snapshot(&root) {
            Ok(snapshot) => sink(GitEvent::SnapshotLoaded { id, snapshot }),
            Err(failure) => sink(GitEvent::Failed { id, operation_applied: false, failure }),
        },
        GitRequest::Diff { root, path, target, .. } => match service.diff(&root, &path, target) {
            Ok(diff) => sink(GitEvent::DiffLoaded { id, diff }),
            Err(failure) => sink(GitEvent::Failed { id, operation_applied: false, failure }),
        },
        GitRequest::Branches { root, .. } => match service.branches(&root) {
            Ok(branches) => sink(GitEvent::BranchesLoaded { id, branches }),
            Err(failure) => sink(GitEvent::Failed { id, operation_applied: false, failure }),
        },
        GitRequest::Execute { root, operation, .. } => match service.execute(&root, &operation) {
            Ok(message) => match service.snapshot(&root) {
                Ok(snapshot) => {
                    sink(GitEvent::OperationFinished { id, operation, message, snapshot });
                }
                Err(failure) => sink(GitEvent::Failed { id, operation_applied: true, failure }),
            },
            Err(failure) => sink(GitEvent::Failed { id, operation_applied: false, failure }),
        },
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    use termesh_core::{
        GitBranch, GitBranchStatus, GitContextDiff, GitDiffTarget, GitEvent, GitFailure,
        GitFailureKind, GitFileDiff, GitOperation, GitRepositorySnapshot, GitRequest, GitRequestId,
        GitResult,
    };

    use crate::{GitService, GitWorker};

    struct RecordingGitService {
        execute: GitResult<String>,
        snapshot: GitResult<GitRepositorySnapshot>,
        calls: Arc<Mutex<Vec<&'static str>>>,
    }

    impl RecordingGitService {
        fn scripted(
            execute: GitResult<String>,
            snapshot: GitResult<GitRepositorySnapshot>,
        ) -> (Self, Arc<Mutex<Vec<&'static str>>>) {
            let calls = Arc::new(Mutex::new(Vec::new()));
            (Self { execute, snapshot, calls: calls.clone() }, calls)
        }
    }

    impl GitService for RecordingGitService {
        fn snapshot(&mut self, _root: &Path) -> GitResult<GitRepositorySnapshot> {
            self.calls.lock().unwrap().push("snapshot");
            self.snapshot.clone()
        }

        fn diff(
            &mut self,
            _root: &Path,
            _path: &Path,
            _target: GitDiffTarget,
        ) -> GitResult<GitFileDiff> {
            unreachable!()
        }

        fn branches(&mut self, _root: &Path) -> GitResult<Vec<GitBranch>> {
            unreachable!()
        }

        fn execute(&mut self, _root: &Path, _operation: &GitOperation) -> GitResult<String> {
            self.calls.lock().unwrap().push("execute");
            self.execute.clone()
        }
    }

    fn snapshot() -> GitRepositorySnapshot {
        GitRepositorySnapshot {
            repository_root: "/repo".into(),
            workspace_root: "/repo".into(),
            branch: GitBranchStatus::default(),
            files: Vec::new(),
            context_diff: GitContextDiff::default(),
        }
    }

    fn failure(message: &str) -> GitFailure {
        GitFailure { kind: GitFailureKind::Command, message: message.into() }
    }

    fn execute_request() -> GitRequest {
        GitRequest::Execute {
            id: GitRequestId::new(7),
            root: "/repo".into(),
            operation: GitOperation::Commit { message: "message".into() },
        }
    }

    #[test]
    fn execute_refreshes_before_reporting_completion() {
        let (service, calls) =
            RecordingGitService::scripted(Ok("committed".into()), Ok(snapshot()));
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = GitWorker::spawn(service, move |event| tx.send(event).unwrap());
        assert!(worker.request(execute_request()));
        assert!(matches!(rx.recv().unwrap(), GitEvent::Started { .. }));
        assert!(matches!(rx.recv().unwrap(), GitEvent::OperationFinished { .. }));
        assert_eq!(*calls.lock().unwrap(), vec!["execute", "snapshot"]);
    }

    #[test]
    fn successful_operation_with_failed_refresh_reports_applied_but_stale() {
        let (service, calls) =
            RecordingGitService::scripted(Ok("committed".into()), Err(failure("refresh failed")));
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = GitWorker::spawn(service, move |event| tx.send(event).unwrap());
        assert!(worker.request(execute_request()));
        assert!(matches!(rx.recv().unwrap(), GitEvent::Started { .. }));
        assert!(matches!(rx.recv().unwrap(), GitEvent::Failed { operation_applied: true, .. }));
        assert_eq!(*calls.lock().unwrap(), vec!["execute", "snapshot"]);
    }

    #[test]
    fn failed_operation_does_not_refresh_and_reports_not_applied() {
        let (service, calls) =
            RecordingGitService::scripted(Err(failure("commit failed")), Ok(snapshot()));
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = GitWorker::spawn(service, move |event| tx.send(event).unwrap());
        assert!(worker.request(execute_request()));
        assert!(matches!(rx.recv().unwrap(), GitEvent::Started { .. }));
        assert!(matches!(rx.recv().unwrap(), GitEvent::Failed { operation_applied: false, .. }));
        assert_eq!(*calls.lock().unwrap(), vec!["execute"]);
    }
}
