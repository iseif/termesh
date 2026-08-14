//! Fixtures, fake services, recorded streams, render snapshots. Phase 00+.
//!
//! Every service trait ships with a fake here so logic is testable without the OS
//! (CONTRIBUTING.md invariants). Keeping them in one crate — rather than behind `cfg(test)`
//! in each service — is what lets `app`-level tests and render snapshots use them too.
#![forbid(unsafe_code)]

pub mod fake_clipboard;
pub mod fake_fs;
pub mod fake_git;
pub mod fake_permission_store;
pub mod fake_tasks;
pub mod scripted_agent;
pub mod scripted_lsp;
pub mod scripted_pty;
pub mod scripted_search;

pub use fake_clipboard::FakeClipboard;
pub use fake_fs::FakeFileSystem;
pub use fake_git::{FakeGitCall, FakeGitControl, FakeGitService};
pub use fake_permission_store::FakePermissionStore;
pub use fake_tasks::FakeTaskService;
pub use scripted_agent::{ScriptedAgent, ScriptedUpdate};
pub use scripted_lsp::{FakeLspCall, FakeLspControl, ScriptedLanguageServer};
pub use scripted_pty::{ScriptedPty, ScriptedPtyControl};
pub use scripted_search::{ScriptedSearch, ScriptedSearchControl};

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use termesh_core::{DirEntryInfo, FsResult};
use termesh_filesystem::FileSystemService;

/// Decorates the in-memory filesystem with algorithmic I/O counters. Tests assert call
/// counts rather than elapsed time, so a loaded CI runner cannot turn a regression into
/// a flaky budget failure.
pub struct CountingFileSystem {
    inner: FakeFileSystem,
    read_dir_calls: AtomicUsize,
    read_file_calls: AtomicUsize,
}

impl CountingFileSystem {
    pub fn new(inner: FakeFileSystem) -> Self {
        Self { inner, read_dir_calls: AtomicUsize::new(0), read_file_calls: AtomicUsize::new(0) }
    }

    pub fn read_dir_calls(&self) -> usize {
        self.read_dir_calls.load(Ordering::Relaxed)
    }

    pub fn read_file_calls(&self) -> usize {
        self.read_file_calls.load(Ordering::Relaxed)
    }
}

impl FileSystemService for CountingFileSystem {
    fn read_dir(&self, path: &Path) -> FsResult<Vec<DirEntryInfo>> {
        self.read_dir_calls.fetch_add(1, Ordering::Relaxed);
        self.inner.read_dir(path)
    }

    fn read_file(&self, path: &Path) -> FsResult<Vec<u8>> {
        self.read_file_calls.fetch_add(1, Ordering::Relaxed);
        self.inner.read_file(path)
    }

    fn create_file(&self, path: &Path) -> FsResult<()> {
        self.inner.create_file(path)
    }

    fn write_file(&self, path: &Path, contents: &[u8]) -> FsResult<()> {
        self.inner.write_file(path, contents)
    }

    fn create_dir(&self, path: &Path) -> FsResult<()> {
        self.inner.create_dir(path)
    }

    fn rename(&self, from: &Path, to: &Path) -> FsResult<()> {
        self.inner.rename(from, to)
    }

    fn remove_file(&self, path: &Path) -> FsResult<()> {
        self.inner.remove_file(path)
    }

    fn remove_dir_all(&self, path: &Path) -> FsResult<()> {
        self.inner.remove_dir_all(path)
    }

    fn canonicalize(&self, path: &Path) -> FsResult<PathBuf> {
        self.inner.canonicalize(path)
    }
}

/// A deep, wide shape whose unexplored branches stand in for an arbitrarily large
/// repository. The laziness tests count directory calls, so fully materialising
/// `width.pow(depth)` nodes would consume memory without strengthening the assertion.
pub fn synthetic_tree(depth: usize, width: usize) -> FakeFileSystem {
    let fs = FakeFileSystem::new();
    fs.add_file("/big/Cargo.toml", b"[package]\nname = \"big\"\nversion = \"0.0.0\"\n");
    let mut trunk = PathBuf::from("/big");
    for level in 0..depth {
        for branch in 0..width {
            let directory = trunk.join(format!("level-{level}-branch-{branch}"));
            fs.add_dir(&directory);
            fs.add_file(directory.join("source.rs"), b"fn synthetic() {}\n");
        }
        trunk.push(format!("level-{level}-branch-0"));
    }
    fs
}

#[cfg(test)]
mod phase_05_search_tests {
    use std::path::PathBuf;
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, Mutex};

    use termesh_core::{SearchEvent, SearchMode, SearchRequest, SearchRequestId};
    use termesh_search::{SearchEventSink, SearchService};

    use crate::ScriptedSearch;

    #[test]
    fn scripted_search_records_requests_and_replays_the_matching_script() {
        let request = SearchRequest {
            id: SearchRequestId::new(8),
            root: PathBuf::from("/repo"),
            mode: SearchMode::Files,
            query: "main".into(),
            limit: 20,
        };
        let event = SearchEvent::Finished { id: request.id, truncated: false };
        let mut search =
            ScriptedSearch::new().with_script(SearchMode::Files, "main", vec![event.clone()]);
        let control = search.control();
        let received = Arc::new(Mutex::new(Vec::new()));
        let received_for_sink = received.clone();
        let sink: SearchEventSink = Arc::new(move |event| {
            received_for_sink.lock().unwrap().push(event);
        });
        search.search(&request, &AtomicBool::new(false), &sink).unwrap();
        assert_eq!(control.requests(), vec![request]);
        assert_eq!(*received.lock().unwrap(), vec![event]);
    }
}
