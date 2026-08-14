use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use termesh_core::{SearchEvent, SearchMatch, SearchMode, SearchRequest};

use crate::{
    literal_matches, parse_rg_json_line, SearchError, SearchEventSink, SearchResult, SearchService,
};

const POLL_INTERVAL: Duration = Duration::from_millis(25);
const STDERR_LIMIT: u64 = 8 * 1024;
const BATCH_LIMIT: usize = 64;

#[derive(Debug, Clone)]
pub struct RealSearch {
    program: String,
    prefix_args: Vec<String>,
    env: Vec<(String, String)>,
    append_search_args: bool,
}

impl RealSearch {
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            prefix_args: Vec::new(),
            env: Vec::new(),
            append_search_args: true,
        }
    }

    #[cfg(test)]
    fn helper(mode: &str) -> Self {
        Self {
            program: std::env::current_exe()
                .expect("current test executable")
                .display()
                .to_string(),
            prefix_args: vec![
                "--exact".into(),
                "real::tests::fake_rg_helper".into(),
                "--nocapture".into(),
            ],
            env: vec![("TERMIDE_FAKE_RG_MODE".into(), mode.into())],
            append_search_args: false,
        }
    }
}

impl Default for RealSearch {
    fn default() -> Self {
        Self::new("rg")
    }
}

impl SearchService for RealSearch {
    fn search(
        &mut self,
        request: &SearchRequest,
        cancelled: &AtomicBool,
        sink: &SearchEventSink,
    ) -> SearchResult<()> {
        let mut command = Command::new(&self.program);
        let search_args = if self.append_search_args { argv(request) } else { Vec::new() };
        command
            .args(&self.prefix_args)
            .args(search_args)
            .envs(self.env.iter().cloned())
            .current_dir(&request.root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        match command.spawn() {
            Ok(mut child) => stream_child(&mut child, request, cancelled, sink),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => match request.mode {
                SearchMode::Files => discover_files(request, cancelled, sink),
                SearchMode::Text => search_text(request, cancelled, sink),
            },
            Err(error) => Err(SearchError::Process(format!("{}: {error}", self.program))),
        }
    }
}

/// `rg --files` is the fast path, not a prerequisite for opening files. The `ignore`
/// walker uses the same gitignore engine as ripgrep, stays on the search worker, and
/// preserves Quick Open on minimal SSH hosts where `rg` is not installed.
fn discover_files(
    request: &SearchRequest,
    cancelled: &AtomicBool,
    sink: &SearchEventSink,
) -> SearchResult<()> {
    let mut batch = Vec::with_capacity(BATCH_LIMIT);
    let mut count = 0usize;
    for entry in ignore::WalkBuilder::new(&request.root).build() {
        if cancelled.load(Ordering::Acquire) || count >= request.limit {
            break;
        }
        let Some(entry) = skippable(entry) else { continue };
        let Some(kind) = entry.file_type() else { continue };
        if !(kind.is_file() || kind.is_symlink()) {
            continue;
        }
        batch.push(SearchMatch { path: entry.into_path(), line: None, column: None, text: None });
        count += 1;
        if batch.len() == BATCH_LIMIT {
            emit_batch(request, sink, &mut batch);
        }
    }
    emit_batch(request, sink, &mut batch);
    Ok(())
}

/// Preserve workspace search on hosts without ripgrep. The walk stays off the UI
/// thread, honors the same ignore files as Quick Open, skips non-UTF-8 content, and
/// reuses the literal matcher so smart-case and character columns stay consistent
/// with matches from dirty editor buffers.
fn search_text(
    request: &SearchRequest,
    cancelled: &AtomicBool,
    sink: &SearchEventSink,
) -> SearchResult<()> {
    let mut batch = Vec::with_capacity(BATCH_LIMIT);
    let mut count = 0usize;
    'files: for entry in ignore::WalkBuilder::new(&request.root).build() {
        if cancelled.load(Ordering::Acquire) || count >= request.limit {
            break;
        }
        let Some(entry) = skippable(entry) else { continue };
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        let Ok(contents) = std::fs::read_to_string(entry.path()) else { continue };
        for found in literal_matches(entry.path(), &contents, &request.query) {
            if cancelled.load(Ordering::Acquire) || count >= request.limit {
                break 'files;
            }
            batch.push(found);
            count += 1;
            if batch.len() == BATCH_LIMIT {
                emit_batch(request, sink, &mut batch);
            }
        }
    }
    emit_batch(request, sink, &mut batch);
    Ok(())
}

/// Drop a walk entry the OS or an ignore file made unreadable instead of failing the
/// whole search. `ignore` reports these as `Err` items — an unreadable directory, a
/// broken symlink, a malformed `.gitignore` line — and ripgrep skips every one of them.
/// Aborting here would make the fallback *more* brittle than the fast path, on exactly
/// the machines that have no `rg` to fall back from.
fn skippable(entry: Result<ignore::DirEntry, ignore::Error>) -> Option<ignore::DirEntry> {
    entry.ok()
}

fn argv(request: &SearchRequest) -> Vec<String> {
    match request.mode {
        SearchMode::Files => vec!["--files".into(), "--color".into(), "never".into()],
        SearchMode::Text => vec![
            "--json".into(),
            "--fixed-strings".into(),
            "--smart-case".into(),
            "--line-number".into(),
            "--column".into(),
            "--color".into(),
            "never".into(),
            "--".into(),
            request.query.clone(),
            ".".into(),
        ],
    }
}

fn stream_child(
    child: &mut Child,
    request: &SearchRequest,
    cancelled: &AtomicBool,
    sink: &SearchEventSink,
) -> SearchResult<()> {
    let stdout =
        child.stdout.take().ok_or_else(|| SearchError::Process("missing stdout".into()))?;
    let stderr =
        child.stderr.take().ok_or_else(|| SearchError::Process("missing stderr".into()))?;
    let (line_tx, line_rx) = mpsc::channel();
    let stdout_handle = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            if line_tx.send(line.map_err(|error| error.to_string())).is_err() {
                break;
            }
        }
    });
    let stderr_handle = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stderr.take(STDERR_LIMIT).read_to_end(&mut bytes);
        bytes
    });

    let mut batch = Vec::with_capacity(BATCH_LIMIT);
    let mut count = 0usize;
    let mut stopped_at_limit = false;
    let mut stream_error = None;
    loop {
        if cancelled.load(Ordering::Acquire) {
            terminate(child);
            break;
        }
        match line_rx.recv_timeout(POLL_INTERVAL) {
            Ok(Ok(line)) => match matches_from_line(request, &line) {
                Ok(found) => {
                    for one in found {
                        if count >= request.limit {
                            stopped_at_limit = true;
                            break;
                        }
                        batch.push(one);
                        count += 1;
                        if batch.len() == BATCH_LIMIT {
                            emit_batch(request, sink, &mut batch);
                        }
                    }
                    if stopped_at_limit {
                        terminate(child);
                        break;
                    }
                }
                Err(error) => {
                    stream_error = Some(error);
                    terminate(child);
                    break;
                }
            },
            Ok(Err(error)) => {
                stream_error = Some(SearchError::Process(error));
                terminate(child);
                break;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if child
                    .try_wait()
                    .map_err(|error| SearchError::Process(error.to_string()))?
                    .is_some()
                {
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    emit_batch(request, sink, &mut batch);
    let status = child.wait().map_err(|error| SearchError::Process(error.to_string()))?;
    let _ = stdout_handle.join();
    let stderr = stderr_handle.join().unwrap_or_default();

    if let Some(error) = stream_error {
        return Err(error);
    }
    if cancelled.load(Ordering::Acquire) || stopped_at_limit {
        return Ok(());
    }
    if matches!(status.code(), Some(0 | 1)) {
        Ok(())
    } else {
        let detail = String::from_utf8_lossy(&stderr).trim().to_owned();
        let detail =
            if detail.is_empty() { format!("ripgrep exited with {status}") } else { detail };
        Err(SearchError::Process(detail))
    }
}

fn terminate(child: &mut Child) {
    let _ = child.kill();
}

fn matches_from_line(request: &SearchRequest, line: &str) -> SearchResult<Vec<SearchMatch>> {
    match request.mode {
        SearchMode::Files => {
            let path = PathBuf::from(line);
            let path = if path.is_absolute() { path } else { request.root.join(path) };
            Ok(vec![SearchMatch { path, line: None, column: None, text: None }])
        }
        SearchMode::Text => parse_rg_json_line(&request.root, line),
    }
}

fn emit_batch(request: &SearchRequest, sink: &SearchEventSink, batch: &mut Vec<SearchMatch>) {
    if !batch.is_empty() {
        sink(SearchEvent::Batch { id: request.id, matches: std::mem::take(batch) });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use std::time::Instant;
    use termesh_core::SearchRequestId;

    fn request(mode: SearchMode, query: &str) -> SearchRequest {
        SearchRequest {
            id: SearchRequestId::new(1),
            root: PathBuf::from("/repo"),
            mode,
            query: query.into(),
            limit: 10,
        }
    }

    fn helper_request() -> SearchRequest {
        let mut request = request(SearchMode::Files, "");
        request.root = std::env::temp_dir();
        request
    }

    #[test]
    fn production_argv_is_structured_and_has_no_shell() {
        assert_eq!(argv(&request(SearchMode::Files, "")), ["--files", "--color", "never"]);
        assert_eq!(
            argv(&request(SearchMode::Text, "two words")),
            [
                "--json",
                "--fixed-strings",
                "--smart-case",
                "--line-number",
                "--column",
                "--color",
                "never",
                "--",
                "two words",
                ".",
            ]
        );
    }

    #[test]
    fn missing_rg_falls_back_to_native_text_search() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let mut request = request(SearchMode::Text, "termesh-search");
        request.root = root.clone();
        let received = Arc::new(Mutex::new(Vec::new()));
        let received_for_sink = received.clone();
        let sink: SearchEventSink = Arc::new(move |event| {
            if let SearchEvent::Batch { matches, .. } = event {
                received_for_sink.lock().unwrap().extend(matches);
            }
        });
        let mut service = RealSearch::new("termesh-certainly-missing-rg");

        service.search(&request, &AtomicBool::new(false), &sink).unwrap();

        assert!(received.lock().unwrap().iter().any(|found| {
            found.path == root.join("Cargo.toml")
                && found.line == Some(2)
                && found.column == Some(9)
                && found.text.as_deref() == Some("name = \"termesh-search\"")
        }));
    }

    #[test]
    fn missing_rg_falls_back_to_native_file_discovery() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let mut request = request(SearchMode::Files, "");
        request.root = root.clone();
        let received = Arc::new(Mutex::new(Vec::new()));
        let received_for_sink = received.clone();
        let sink: SearchEventSink = Arc::new(move |event| {
            if let SearchEvent::Batch { matches, .. } = event {
                received_for_sink.lock().unwrap().extend(matches);
            }
        });
        let mut service = RealSearch::new("termesh-certainly-missing-rg");

        service.search(&request, &AtomicBool::new(false), &sink).unwrap();

        let paths = received.lock().unwrap();
        assert!(paths.iter().any(|found| found.path == root.join("Cargo.toml")));
        assert!(paths.iter().any(|found| found.path == root.join("src/lib.rs")));
    }

    /// A directory the walker cannot read must cost us that directory, not the search.
    /// Without `rg` installed this walk *is* Quick Open, so one unreadable folder
    /// aborting it would leave the user with no file finder at all.
    #[cfg(unix)]
    #[test]
    fn an_unreadable_directory_is_skipped_rather_than_failing_the_walk() {
        use std::os::unix::fs::PermissionsExt;

        let root = std::env::temp_dir().join("termesh-search-unreadable");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("locked")).unwrap();
        std::fs::write(root.join("visible.txt"), "hello\n").unwrap();
        std::fs::write(root.join("locked/hidden.txt"), "hello\n").unwrap();
        std::fs::set_permissions(root.join("locked"), std::fs::Permissions::from_mode(0o000))
            .unwrap();

        let mut request = request(SearchMode::Files, "");
        request.root = root.clone();
        let received = Arc::new(Mutex::new(Vec::new()));
        let received_for_sink = received.clone();
        let sink: SearchEventSink = Arc::new(move |event| {
            if let SearchEvent::Batch { matches, .. } = event {
                received_for_sink.lock().unwrap().extend(matches);
            }
        });

        let outcome = discover_files(&request, &AtomicBool::new(false), &sink);

        // Restore before asserting so a failure still leaves a removable directory.
        std::fs::set_permissions(root.join("locked"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        let found = received.lock().unwrap().clone();
        std::fs::remove_dir_all(&root).unwrap();

        assert!(outcome.is_ok(), "one unreadable directory must not fail the walk");
        assert!(found.iter().any(|entry| entry.path == root.join("visible.txt")));
    }

    #[test]
    fn fake_rg_helper() {
        match std::env::var("TERMIDE_FAKE_RG_MODE").as_deref() {
            Ok("lines") => {
                println!("src/lib.rs");
                println!("src/main.rs");
            }
            Ok("block") => loop {
                std::thread::sleep(Duration::from_millis(50));
            },
            _ => {}
        }
    }

    #[test]
    fn production_child_path_streams_stdout() {
        let mut service = RealSearch::helper("lines");
        let received = Arc::new(Mutex::new(Vec::new()));
        let received_for_sink = received.clone();
        let sink: SearchEventSink = Arc::new(move |event| {
            if let SearchEvent::Batch { matches, .. } = event {
                received_for_sink.lock().unwrap().extend(matches);
            }
        });
        service.search(&helper_request(), &AtomicBool::new(false), &sink).unwrap();
        let paths = received.lock().unwrap();
        assert!(paths.iter().any(|found| found.path.ends_with("src/lib.rs")));
        assert!(paths.iter().any(|found| found.path.ends_with("src/main.rs")));
    }

    #[test]
    fn cancellation_kills_and_waits_for_the_child() {
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancelled_for_search = cancelled.clone();
        let started = Instant::now();
        let handle = std::thread::spawn(move || {
            let mut service = RealSearch::helper("block");
            let sink: SearchEventSink = Arc::new(|_| {});
            service.search(&helper_request(), &cancelled_for_search, &sink)
        });
        std::thread::sleep(Duration::from_millis(100));
        cancelled.store(true, Ordering::Release);
        handle.join().unwrap().unwrap();
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}
