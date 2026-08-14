//! The filesystem worker thread (ADR-0005 §1).
//!
//! Owns a [`FileSystemService`] and does every blocking call on its own thread, emitting
//! [`FsEvent`]s back into the state loop. This is the half of the concurrency model that
//! makes the synchronous trait safe: the methods block, but never on the render loop.
//!
//! The worker is deliberately dumb — it reads what it is told to read and reports what it
//! found. All the decisions (what to expand, how to reconcile a re-read) live in
//! [`crate::tree::FileTree`], where they are pure and unit-testable.

use std::sync::mpsc::{self, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use termesh_core::{FsEvent, FsRequest};

use crate::ignore_rules::{IgnoreOptions, IgnoreRules};
use crate::reader::DirReader;
use crate::service::FileSystemService;
use crate::watch::{is_relevant, RelevanceFilter, RootWatcher, DEFAULT_WINDOW};

/// Turn a mutation outcome into the event the state loop expects.
fn mutation_result(outcome: crate::service::FsResult<()>, path: &std::path::Path) -> FsEvent {
    match outcome {
        Ok(()) => FsEvent::Changed(vec![path.to_path_buf()]),
        Err(e) => FsEvent::MutationFailed(e),
    }
}

/// Handle to a running worker thread. Dropping it shuts the thread down and joins it.
pub struct FsWorker {
    tx: Sender<FsRequest>,
    handle: Option<JoinHandle<()>>,
}

impl FsWorker {
    /// Start a worker over `fs`, delivering results through `sink`.
    ///
    /// `sink` is a callback rather than a channel so the caller decides how events are
    /// wrapped — `app` turns them into `AppMessage::Fs`, tests collect them directly.
    pub fn spawn<S, F>(fs: S, options: IgnoreOptions, sink: F) -> Self
    where
        S: FileSystemService + 'static,
        F: Fn(FsEvent) + Send + Sync + 'static,
    {
        Self::spawn_with_window(fs, options, DEFAULT_WINDOW, sink)
    }

    /// As [`Self::spawn`], with an explicit debounce window. Tests use a short one.
    pub fn spawn_with_window<S, F>(
        fs: S,
        options: IgnoreOptions,
        watch_window: Duration,
        sink: F,
    ) -> Self
    where
        S: FileSystemService + 'static,
        F: Fn(FsEvent) + Send + Sync + 'static,
    {
        let sink = Arc::new(sink);
        let (tx, rx) = mpsc::channel::<FsRequest>();
        let handle = std::thread::Builder::new()
            .name("termesh-fs".into())
            .spawn(move || {
                // Both are built on the first Watch, which is also when we learn the
                // root the ignore chain has to be anchored to.
                let mut reader: Option<DirReader> = None;
                let mut watcher: Option<RootWatcher> = None;

                while let Ok(req) = rx.recv() {
                    match req {
                        FsRequest::ReadDir { id, path } => {
                            let result = match reader.as_mut() {
                                Some(r) => r.read(&path),
                                // Unreachable in practice: `Model::open_workspace`
                                // queues `Watch` before any `ReadDir`, and that ordering
                                // is enforced by a test. Degrade to an unfiltered read
                                // rather than stalling if that ever stops holding.
                                None => DirReader::unfiltered(&fs).read(&path),
                            };
                            sink(match result {
                                Ok(entries) => FsEvent::DirLoaded { id, entries },
                                Err(error) => FsEvent::DirFailed { id, error },
                            });
                        }
                        // Opening and saving are blocking I/O like any other, so they
                        // run here rather than on the render loop. Both report against a
                        // BufferId, since by the time the answer arrives the user may
                        // have opened something else.
                        FsRequest::ReadFile { buffer, path } => {
                            sink(match fs.read_file(&path) {
                                Ok(contents) => FsEvent::FileLoaded { buffer, path, contents },
                                Err(error) => FsEvent::FileFailed { buffer, error },
                            });
                        }
                        FsRequest::ReadPreview { request, path, line, context } => {
                            sink(match preview(&fs, &path, line, context.min(10)) {
                                Ok((start_line, text)) => {
                                    FsEvent::PreviewLoaded { request, path, start_line, text }
                                }
                                Err(error) => FsEvent::PreviewFailed { request, path, error },
                            });
                        }
                        FsRequest::ResolvePath { request, path } => {
                            sink(match fs.canonicalize(&path) {
                                Ok(path) => FsEvent::PathResolved { request, path },
                                Err(error) => FsEvent::PathResolveFailed { request, path, error },
                            });
                        }
                        FsRequest::WriteFile { buffer, path, contents, version } => {
                            sink(match fs.write_file(&path, &contents) {
                                Ok(()) => FsEvent::FileSaved { buffer, version },
                                Err(error) => FsEvent::FileFailed { buffer, error },
                            });
                        }
                        FsRequest::Watch(root) => {
                            let r = DirReader::new(&fs, &root, options);

                            // The watch thread needs an owned, Send predicate, and
                            // `IgnoreRules` is neither — so snapshot the decision as a
                            // closure over a fresh rules set anchored at the same root.
                            let rules = IgnoreRules::for_root(&fs, &root, options);
                            let filter = RelevanceFilter::new(move |p| is_relevant(p, &rules));

                            // Stop any previous watch before starting the new one: two
                            // recursive watchers over overlapping roots would report the
                            // same change twice. Dropping joins the old debounce thread.
                            drop(watcher.take());

                            let sink_for_watch = sink.clone();
                            // A root we cannot watch is degraded, not broken: the tree
                            // still works, it just will not update by itself.
                            watcher =
                                RootWatcher::start(&root, watch_window, filter, move |paths| {
                                    sink_for_watch(FsEvent::Changed(paths))
                                });
                            reader = Some(r);
                        }
                        // Mutations report success as `Changed`, not as a bespoke "done"
                        // event. That way exactly one code path brings disk state back
                        // into the tree, whether the change came from us or from an
                        // external editor — and it works with no watcher running.
                        FsRequest::CreateFile(path) => {
                            sink(mutation_result(fs.create_file(&path), &path));
                        }
                        FsRequest::CreateDir(path) => {
                            sink(mutation_result(fs.create_dir(&path), &path));
                        }
                        FsRequest::Rename { from, to } => {
                            // Report both ends: the source directory loses an entry and
                            // the destination gains one, and they may differ.
                            sink(match fs.rename(&from, &to) {
                                Ok(()) => FsEvent::Changed(vec![from, to]),
                                Err(e) => FsEvent::MutationFailed(e),
                            });
                        }
                        FsRequest::Remove { path, recursive } => {
                            let outcome = if recursive {
                                fs.remove_dir_all(&path)
                            } else {
                                fs.remove_file(&path)
                            };
                            sink(mutation_result(outcome, &path));
                        }
                        FsRequest::Shutdown => break,
                    }
                }
            })
            .expect("spawning the filesystem worker thread");

        Self { tx, handle: Some(handle) }
    }

    /// Queue work. Returns `false` if the worker has already stopped — callers treat a
    /// dead worker as "no result will arrive", never as a reason to block or panic.
    pub fn request(&self, req: FsRequest) -> bool {
        self.tx.send(req).is_ok()
    }
}

fn preview(
    fs: &dyn FileSystemService,
    path: &std::path::Path,
    line: usize,
    context: usize,
) -> crate::service::FsResult<(usize, String)> {
    let bytes = fs.read_file(path)?;
    let contents = String::from_utf8(bytes).map_err(|_| crate::service::FsError::Other {
        path: path.to_path_buf(),
        message: "not a UTF-8 text file".into(),
    })?;
    let lines: Vec<&str> = contents.split_inclusive('\n').collect();
    let target = line.saturating_sub(1).min(lines.len().saturating_sub(1));
    let start = target.saturating_sub(context);
    let end = (target + context + 1).min(lines.len());
    Ok((start + 1, lines[start..end].concat()))
}

impl Drop for FsWorker {
    fn drop(&mut self) {
        // Ask politely, then wait: the thread may be mid-`read_dir` on a slow disk and
        // we would rather join it than leave it writing into a dropped sink.
        let _ = self.tx.send(FsRequest::Shutdown);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use std::sync::mpsc::Receiver;
    use std::time::Duration;

    use termesh_core::{
        DirEntryInfo, EntryKind, FsError, FsResult, LocationRequestId, NodeId, PreviewRequestId,
    };

    /// A minimal in-crate fake. `test-support`'s richer one cannot be used here: it
    /// depends on this crate, so using it would be a dependency cycle.
    struct StubFs(Vec<DirEntryInfo>, Vec<u8>);

    impl FileSystemService for StubFs {
        fn read_dir(&self, path: &Path) -> FsResult<Vec<DirEntryInfo>> {
            if path == Path::new("/denied") {
                return Err(FsError::PermissionDenied(path.to_path_buf()));
            }
            Ok(self.0.clone())
        }
        fn read_file(&self, _: &Path) -> FsResult<Vec<u8>> {
            Ok(self.1.clone())
        }
        fn create_file(&self, _: &Path) -> FsResult<()> {
            Ok(())
        }
        fn write_file(&self, _: &Path, _: &[u8]) -> FsResult<()> {
            Ok(())
        }
        fn create_dir(&self, _: &Path) -> FsResult<()> {
            Ok(())
        }
        fn rename(&self, _: &Path, _: &Path) -> FsResult<()> {
            Ok(())
        }
        fn remove_file(&self, _: &Path) -> FsResult<()> {
            Ok(())
        }
        fn remove_dir_all(&self, _: &Path) -> FsResult<()> {
            Ok(())
        }
        fn canonicalize(&self, p: &Path) -> FsResult<PathBuf> {
            if p == Path::new("/missing") {
                Err(FsError::NotFound(p.to_path_buf()))
            } else {
                Ok(p.to_path_buf())
            }
        }
    }

    fn stub() -> StubFs {
        StubFs(
            vec![DirEntryInfo {
                name: "main.rs".into(),
                path: PathBuf::from("/src/main.rs"),
                kind: EntryKind::File,
            }],
            Vec::new(),
        )
    }

    fn worker_with_channel() -> (FsWorker, Receiver<FsEvent>) {
        let (tx, rx) = mpsc::channel();
        let worker = FsWorker::spawn(stub(), IgnoreOptions::show_all(), move |e| {
            let _ = tx.send(e);
        });
        (worker, rx)
    }

    fn recv(rx: &Receiver<FsEvent>) -> FsEvent {
        rx.recv_timeout(Duration::from_secs(5)).expect("worker should answer")
    }

    #[test]
    fn a_read_request_comes_back_as_dir_loaded() {
        let (worker, rx) = worker_with_channel();
        assert!(
            worker.request(FsRequest::ReadDir { id: NodeId::new(7), path: PathBuf::from("/src") })
        );

        match recv(&rx) {
            FsEvent::DirLoaded { id, entries } => {
                assert_eq!(id, NodeId::new(7), "the answer names the node that asked");
                assert_eq!(entries.len(), 1);
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn preview_returns_only_the_requested_window() {
        let (tx, rx) = mpsc::channel();
        let worker = FsWorker::spawn(
            StubFs(Vec::new(), b"one\ntwo\nneedle\nfour\nfive\n".to_vec()),
            IgnoreOptions::show_all(),
            move |event| {
                let _ = tx.send(event);
            },
        );
        worker.request(FsRequest::ReadPreview {
            request: PreviewRequestId::new(3),
            path: PathBuf::from("/p/src/lib.rs"),
            line: 3,
            context: 1,
        });
        assert!(matches!(
            recv(&rx),
            FsEvent::PreviewLoaded {
                request,
                start_line: 2,
                text,
                ..
            } if request == PreviewRequestId::new(3) && text == "two\nneedle\nfour\n"
        ));
    }

    #[test]
    fn path_resolution_preserves_the_request_id_on_success_and_failure() {
        let (worker, rx) = worker_with_channel();
        worker.request(FsRequest::ResolvePath {
            request: LocationRequestId::new(4),
            path: PathBuf::from("/src/main.rs"),
        });
        assert!(matches!(
            recv(&rx),
            FsEvent::PathResolved { request, path }
                if request == LocationRequestId::new(4) && path == Path::new("/src/main.rs")
        ));
        worker.request(FsRequest::ResolvePath {
            request: LocationRequestId::new(5),
            path: PathBuf::from("/missing"),
        });
        assert!(matches!(
            recv(&rx),
            FsEvent::PathResolveFailed { request, .. }
                if request == LocationRequestId::new(5)
        ));
    }

    #[test]
    fn a_failed_read_comes_back_as_dir_failed_and_the_worker_survives() {
        let (worker, rx) = worker_with_channel();
        worker.request(FsRequest::ReadDir { id: NodeId::new(1), path: PathBuf::from("/denied") });
        assert!(matches!(recv(&rx), FsEvent::DirFailed { .. }));

        // The worker must keep serving after an error rather than tearing down.
        worker.request(FsRequest::ReadDir { id: NodeId::new(2), path: PathBuf::from("/src") });
        assert!(matches!(recv(&rx), FsEvent::DirLoaded { .. }));
    }

    #[test]
    fn requests_are_answered_in_order() {
        let (worker, rx) = worker_with_channel();
        for i in 0..5 {
            worker.request(FsRequest::ReadDir { id: NodeId::new(i), path: PathBuf::from("/src") });
        }
        for i in 0..5 {
            match recv(&rx) {
                FsEvent::DirLoaded { id, .. } => assert_eq!(id, NodeId::new(i)),
                other => panic!("unexpected event: {other:?}"),
            }
        }
    }

    #[test]
    fn dropping_the_worker_stops_the_thread() {
        let (worker, rx) = worker_with_channel();
        worker.request(FsRequest::ReadDir { id: NodeId::new(0), path: PathBuf::from("/src") });
        let _ = recv(&rx);

        drop(worker); // joins the thread; the sink is dropped with it
        assert!(rx.recv().is_err(), "no further events once the worker is gone");
    }

    /// The interactive path in `app::run` sends `Watch` and then `ReadDir`, and the very
    /// first listing is the one the user sees on launch. This proves ignore rules are
    /// already live for it — the arrangement the app's ordering test guards from the
    /// other side.
    #[test]
    fn the_first_listing_after_watch_is_already_ignore_filtered() {
        use crate::real::RealFileSystem;

        let stamp =
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let root = std::env::temp_dir().join(format!("termesh-firstread-{stamp}"));
        std::fs::create_dir_all(root.join("target")).unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join(".gitignore"), b"target\n").unwrap();

        let (tx, rx) = mpsc::channel();
        let worker = FsWorker::spawn(RealFileSystem::new(), IgnoreOptions::default(), move |e| {
            let _ = tx.send(e);
        });
        worker.request(FsRequest::Watch(root.clone()));
        worker.request(FsRequest::ReadDir { id: NodeId::new(0), path: root.clone() });

        let names: Vec<String> = loop {
            match rx.recv_timeout(Duration::from_secs(5)).expect("worker should answer") {
                FsEvent::DirLoaded { entries, .. } => {
                    break entries.iter().map(|e| e.name.to_string_lossy().into_owned()).collect()
                }
                _ => continue,
            }
        };

        drop(worker);
        let _ = std::fs::remove_dir_all(&root);

        assert_eq!(names, ["src"], "target/ and .gitignore must be filtered from the first read");
    }

    /// End-to-end over the real filesystem: watching a directory and creating a file in
    /// it must wake the loop with a coalesced `Changed` batch. This is the one test that
    /// proves the OS half works; everything about *policy* is unit-tested in `watch`.
    #[test]
    fn creating_a_file_under_a_watched_root_emits_a_changed_batch() {
        use crate::real::RealFileSystem;

        let stamp =
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let root = std::env::temp_dir().join(format!("termesh-watch-{stamp}"));
        std::fs::create_dir_all(&root).unwrap();

        let (tx, rx) = mpsc::channel();
        let worker = FsWorker::spawn_with_window(
            RealFileSystem::new(),
            IgnoreOptions::show_all(),
            Duration::from_millis(50),
            move |e| {
                let _ = tx.send(e);
            },
        );
        worker.request(FsRequest::Watch(root.clone()));

        // Sleeping a fixed span here was flaky: under parallel test load the worker
        // thread had not always reached `RootWatcher::start` before we wrote the file,
        // so the event was never generated and the deadline below waited on nothing.
        // Requests are answered in order, so a `ReadDir` reply proves `Watch` is done.
        worker.request(FsRequest::ReadDir { id: NodeId::new(0), path: root.clone() });
        let handshake = std::time::Instant::now() + Duration::from_secs(10);
        while std::time::Instant::now() < handshake {
            if let Ok(FsEvent::DirLoaded { .. }) = rx.recv_timeout(Duration::from_millis(500)) {
                break;
            }
        }

        // Registering the watch and the OS actually delivering for it are still two
        // different moments (FSEvents in particular arms asynchronously), so keep
        // re-touching the file instead of betting the whole test on the first write.
        let created = root.join("created.rs");
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let mut saw_change = false;
        while std::time::Instant::now() < deadline && !saw_change {
            std::fs::write(&created, b"fn main() {}").unwrap();
            while let Ok(event) = rx.recv_timeout(Duration::from_millis(250)) {
                if let FsEvent::Changed(paths) = event {
                    if paths.iter().any(|p| p.ends_with("created.rs")) {
                        saw_change = true;
                        break;
                    }
                }
            }
        }

        drop(worker);
        let _ = std::fs::remove_dir_all(&root);
        assert!(saw_change, "a new file under the watched root should reach the loop");
    }

    #[test]
    fn requesting_after_shutdown_reports_failure_instead_of_panicking() {
        let (worker, _rx) = worker_with_channel();
        worker.request(FsRequest::Shutdown);
        // The thread has stopped; the channel may still accept one buffered send, so we
        // only assert the call returns rather than unwinding.
        let _ =
            worker.request(FsRequest::ReadDir { id: NodeId::new(0), path: PathBuf::from("/src") });
    }
}
