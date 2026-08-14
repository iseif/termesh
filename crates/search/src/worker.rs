use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use termesh_core::{SearchEvent, SearchMode, SearchRequest};

use crate::{SearchEventSink, SearchService};

const MAX_BATCH: usize = 64;
pub const DEFAULT_TEXT_DEBOUNCE: Duration = Duration::from_millis(75);

enum WorkerRequest {
    Search { request: SearchRequest, cancelled: Arc<AtomicBool> },
    Shutdown,
}

/// Owns the one active search and serializes replacement requests.
pub struct SearchWorker {
    tx: mpsc::Sender<WorkerRequest>,
    active: Arc<Mutex<Option<Arc<AtomicBool>>>>,
    handle: Option<JoinHandle<()>>,
}

impl SearchWorker {
    pub fn spawn<S, F>(service: S, sink: F) -> Self
    where
        S: SearchService,
        F: Fn(SearchEvent) + Send + Sync + 'static,
    {
        Self::spawn_with_debounce(service, DEFAULT_TEXT_DEBOUNCE, sink)
    }

    pub fn spawn_with_debounce<S, F>(mut service: S, debounce: Duration, sink: F) -> Self
    where
        S: SearchService,
        F: Fn(SearchEvent) + Send + Sync + 'static,
    {
        let sink: SearchEventSink = Arc::new(sink);
        let (tx, rx) = mpsc::channel();
        let active = Arc::new(Mutex::new(None));
        let worker_active = active.clone();
        let handle = std::thread::Builder::new()
            .name("termesh-search".into())
            .spawn(move || worker_loop(&mut service, rx, debounce, sink, worker_active))
            .expect("spawning the search worker thread");
        Self { tx, active, handle: Some(handle) }
    }

    /// Queue a replacement search, cancelling whichever request currently owns the
    /// service. Returns false only after the worker has shut down.
    pub fn request(&self, request: SearchRequest) -> bool {
        let cancelled = Arc::new(AtomicBool::new(false));
        {
            let mut active = self.active.lock().expect("search cancellation state poisoned");
            if let Some(previous) = active.replace(cancelled.clone()) {
                previous.store(true, Ordering::Release);
            }
        }
        if self.tx.send(WorkerRequest::Search { request, cancelled: cancelled.clone() }).is_ok() {
            true
        } else {
            cancelled.store(true, Ordering::Release);
            false
        }
    }

    pub fn cancel(&self) -> bool {
        let active = self.active.lock().expect("search cancellation state poisoned").clone();
        if let Some(active) = active {
            active.store(true, Ordering::Release);
            true
        } else {
            false
        }
    }
}

impl Drop for SearchWorker {
    fn drop(&mut self) {
        if let Some(active) = self.active.lock().expect("search cancellation state poisoned").take()
        {
            active.store(true, Ordering::Release);
        }
        let _ = self.tx.send(WorkerRequest::Shutdown);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn worker_loop<S: SearchService>(
    service: &mut S,
    rx: mpsc::Receiver<WorkerRequest>,
    debounce: Duration,
    sink: SearchEventSink,
    active: Arc<Mutex<Option<Arc<AtomicBool>>>>,
) {
    while let Ok(message) = rx.recv() {
        let WorkerRequest::Search { request, cancelled } = message else { break };
        let Some((request, cancelled)) = debounce_request(request, cancelled, debounce, &rx, &sink)
        else {
            break;
        };
        run_request(service, &request, &cancelled, &sink);
        let mut current = active.lock().expect("search cancellation state poisoned");
        if current.as_ref().is_some_and(|token| Arc::ptr_eq(token, &cancelled)) {
            current.take();
        }
    }
}

fn debounce_request(
    mut request: SearchRequest,
    mut cancelled: Arc<AtomicBool>,
    debounce: Duration,
    rx: &mpsc::Receiver<WorkerRequest>,
    sink: &SearchEventSink,
) -> Option<(SearchRequest, Arc<AtomicBool>)> {
    if request.mode != SearchMode::Text || debounce.is_zero() {
        return Some((request, cancelled));
    }

    let mut deadline = Instant::now() + debounce;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(remaining) {
            Ok(WorkerRequest::Search { request: replacement, cancelled: replacement_token }) => {
                sink(SearchEvent::Cancelled { id: request.id });
                request = replacement;
                cancelled = replacement_token;
                if request.mode != SearchMode::Text {
                    return Some((request, cancelled));
                }
                deadline = Instant::now() + debounce;
            }
            Ok(WorkerRequest::Shutdown) | Err(mpsc::RecvTimeoutError::Disconnected) => return None,
            Err(mpsc::RecvTimeoutError::Timeout) => return Some((request, cancelled)),
        }
    }
}

fn run_request<S: SearchService>(
    service: &mut S,
    request: &SearchRequest,
    cancelled: &Arc<AtomicBool>,
    sink: &SearchEventSink,
) {
    if cancelled.load(Ordering::Acquire) {
        sink(SearchEvent::Cancelled { id: request.id });
        return;
    }

    sink(SearchEvent::Started { id: request.id });
    let forwarded = Arc::new(AtomicUsize::new(0));
    let forwarded_for_sink = forwarded.clone();
    let outer = sink.clone();
    let id = request.id;
    let limit = request.limit;
    let bounded_sink: SearchEventSink = Arc::new(move |event| {
        let SearchEvent::Batch { id: batch_id, matches } = event else { return };
        if batch_id != id {
            return;
        }
        let already = forwarded_for_sink.load(Ordering::Acquire);
        let remaining = limit.saturating_sub(already);
        for chunk in matches.into_iter().take(remaining).collect::<Vec<_>>().chunks(MAX_BATCH) {
            outer(SearchEvent::Batch { id, matches: chunk.to_vec() });
            forwarded_for_sink.fetch_add(chunk.len(), Ordering::AcqRel);
        }
    });

    let result = service.search(request, cancelled, &bounded_sink);
    let count = forwarded.load(Ordering::Acquire);
    if cancelled.load(Ordering::Acquire) {
        sink(SearchEvent::Cancelled { id: request.id });
    } else if let Err(error) = result {
        sink(SearchEvent::Failed {
            id: request.id,
            message: error.to_string(),
            partial: count > 0,
        });
    } else {
        sink(SearchEvent::Finished { id: request.id, truncated: count >= request.limit });
    }
}
