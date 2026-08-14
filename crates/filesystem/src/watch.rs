//! File watching, and the coalescing policy that makes it usable (ADR-0005 §5).
//!
//! `notify` reports raw OS events, and a single "save" in an editor can produce half a
//! dozen of them (write a temp file, rename it over the target, delete the backup). Left
//! raw, that is one full directory re-read per event.
//!
//! We coalesce in our own code rather than pulling in `notify-debouncer-full`: one
//! dependency instead of two, the debouncer's surface has moved across `notify` major
//! versions, and — the real reason — a policy we own is a policy we can unit-test by
//! feeding it synthetic batches with a synthetic clock, which is exactly what the tests
//! below do.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use notify::{RecursiveMode, Watcher};

#[cfg(not(test))]
type ActiveWatcher = notify::RecommendedWatcher;
#[cfg(test)]
type ActiveWatcher = notify::PollWatcher;

use crate::ignore_rules::IgnoreRules;

/// How long to gather events before emitting a batch. Long enough to absorb an editor's
/// save dance, short enough that the tree feels live.
pub const DEFAULT_WINDOW: Duration = Duration::from_millis(100);

/// Accumulates changed paths and releases them once the window has elapsed.
///
/// Deliberately holds no clock of its own: the caller supplies `now`, which is what makes
/// the policy testable without sleeping.
#[derive(Debug)]
pub struct Coalescer {
    window: Duration,
    /// A set, so a burst of edits to one path collapses to one entry. Ordered so batches
    /// are deterministic.
    pending: BTreeSet<PathBuf>,
    opened_at: Option<Instant>,
}

impl Coalescer {
    pub fn new(window: Duration) -> Self {
        Self { window, pending: BTreeSet::new(), opened_at: None }
    }

    /// Note a changed path. The window starts on the first path of a batch.
    pub fn push(&mut self, path: PathBuf, now: Instant) {
        if self.pending.is_empty() {
            self.opened_at = Some(now);
        }
        self.pending.insert(path);
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    /// How long until the current batch is ready, if one is open.
    pub fn time_remaining(&self, now: Instant) -> Option<Duration> {
        let opened = self.opened_at?;
        Some(self.window.saturating_sub(now.duration_since(opened)))
    }

    /// Take the batch if the window has elapsed. Returns `None` while it is still open,
    /// so a steady trickle of events still gets released on schedule rather than being
    /// deferred forever by each new arrival.
    pub fn take_if_ready(&mut self, now: Instant) -> Option<Vec<PathBuf>> {
        let opened = self.opened_at?;
        if now.duration_since(opened) < self.window {
            return None;
        }
        Some(self.take())
    }

    /// Take whatever is pending regardless of the window (used on shutdown).
    pub fn take(&mut self) -> Vec<PathBuf> {
        self.opened_at = None;
        std::mem::take(&mut self.pending).into_iter().collect()
    }
}

/// Paths that are noise no matter what the ignore rules say: editor swap and backup
/// files, which appear and vanish around every save (ADR-0005 §5).
pub fn is_editor_noise(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else { return false };
    name.ends_with('~')
        || name.ends_with(".swp")
        || name.ends_with(".swx")
        || name.ends_with(".tmp")
        // Vim's fsync probe file.
        || name == "4913"
        // Emacs lock files.
        || name.starts_with(".#")
}

/// Whether a raw watch event is worth waking the tree for.
pub fn is_relevant(path: &Path, rules: &IgnoreRules) -> bool {
    if is_editor_noise(path) {
        return false;
    }
    // We cannot stat the path (it may already be gone), so ask about it as a file *and*
    // as a directory; only drop it when it is ignored either way.
    !(rules.is_hidden(path, false) && rules.is_hidden(path, true))
}

/// A running recursive watch over one root.
///
/// Owns both the `notify` watcher and the debounce thread; dropping it stops both.
pub struct RootWatcher {
    _watcher: ActiveWatcher,
    stop: mpsc::Sender<()>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl RootWatcher {
    /// Start watching `root` recursively, emitting coalesced path batches to `sink`.
    ///
    /// Returns `None` if the OS refuses the watch (too many watches, unreadable root) —
    /// a workspace without live updates is degraded, not broken, so the caller carries on.
    pub fn start<F>(root: &Path, window: Duration, filter: RelevanceFilter, sink: F) -> Option<Self>
    where
        F: Fn(Vec<PathBuf>) + Send + 'static,
    {
        let (raw_tx, raw_rx) = mpsc::channel::<PathBuf>();
        let event_handler = move |res: notify::Result<notify::Event>| {
            if let Ok(event) = res {
                for path in event.paths {
                    // A dead receiver means we are shutting down; nothing to do.
                    let _ = raw_tx.send(path);
                }
            }
        };
        #[cfg(not(test))]
        let mut watcher = notify::recommended_watcher(event_handler).ok()?;
        // Sandboxed CI environments can suppress host event facilities such as macOS
        // FSEvents even though watcher registration succeeds. Polling in unit-test
        // builds keeps this integration test deterministic while production continues
        // to use the platform-recommended event backend.
        #[cfg(test)]
        let mut watcher = notify::PollWatcher::new(
            event_handler,
            notify::Config::default().with_poll_interval(window),
        )
        .ok()?;
        watcher.watch(root, RecursiveMode::Recursive).ok()?;

        let (stop, stop_rx) = mpsc::channel::<()>();
        let handle = std::thread::Builder::new()
            .name("termesh-fs-watch".into())
            .spawn(move || {
                let mut coalescer = Coalescer::new(window);
                loop {
                    if stop_rx.try_recv().is_ok() {
                        return;
                    }
                    // Wait only as long as the open batch has left to run, so a batch is
                    // released on time even if no further events arrive.
                    let wait = coalescer.time_remaining(Instant::now()).unwrap_or(window);
                    match raw_rx.recv_timeout(wait) {
                        Ok(path) => {
                            if filter.accepts(&path) {
                                coalescer.push(path, Instant::now());
                            }
                        }
                        Err(RecvTimeoutError::Timeout) => {}
                        // The watcher is gone; flush anything held and stop.
                        Err(RecvTimeoutError::Disconnected) => {
                            if !coalescer.is_empty() {
                                sink(coalescer.take());
                            }
                            return;
                        }
                    }
                    if let Some(batch) = coalescer.take_if_ready(Instant::now()) {
                        if !batch.is_empty() {
                            sink(batch);
                        }
                    }
                }
            })
            .ok()?;

        Some(Self { _watcher: watcher, stop, handle: Some(handle) })
    }
}

impl Drop for RootWatcher {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// Decides which raw watch paths reach the coalescer.
///
/// A boxed predicate rather than the `IgnoreRules` itself, because the rules are not
/// `Send` and the watch thread needs something it can own.
pub struct RelevanceFilter(Box<dyn Fn(&Path) -> bool + Send>);

impl RelevanceFilter {
    pub fn new<F: Fn(&Path) -> bool + Send + 'static>(f: F) -> Self {
        Self(Box::new(f))
    }

    /// Drop only editor noise. Used when no ignore rules are anchored yet.
    pub fn noise_only() -> Self {
        Self::new(|p| !is_editor_noise(p))
    }

    pub fn accepts(&self, path: &Path) -> bool {
        (self.0)(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window() -> Duration {
        Duration::from_millis(100)
    }

    #[test]
    fn nothing_is_released_before_the_window_elapses() {
        let t0 = Instant::now();
        let mut c = Coalescer::new(window());
        c.push("/r/a.rs".into(), t0);
        assert_eq!(c.take_if_ready(t0 + Duration::from_millis(50)), None);
    }

    #[test]
    fn the_batch_is_released_once_the_window_elapses() {
        let t0 = Instant::now();
        let mut c = Coalescer::new(window());
        c.push("/r/a.rs".into(), t0);
        assert_eq!(
            c.take_if_ready(t0 + Duration::from_millis(120)),
            Some(vec![PathBuf::from("/r/a.rs")])
        );
    }

    #[test]
    fn repeated_events_for_one_path_collapse_to_a_single_entry() {
        let t0 = Instant::now();
        let mut c = Coalescer::new(window());
        for _ in 0..50 {
            c.push("/r/a.rs".into(), t0);
        }
        let batch = c.take_if_ready(t0 + Duration::from_millis(120)).unwrap();
        assert_eq!(batch, vec![PathBuf::from("/r/a.rs")], "one save, one entry");
    }

    #[test]
    fn a_save_storm_across_files_becomes_one_batch() {
        let t0 = Instant::now();
        let mut c = Coalescer::new(window());
        // An editor's save dance plus a formatter touching siblings.
        for (i, p) in ["/r/a.rs", "/r/b.rs", "/r/c.rs", "/r/a.rs"].iter().enumerate() {
            c.push(PathBuf::from(p), t0 + Duration::from_millis(i as u64 * 10));
        }
        let batch = c.take_if_ready(t0 + Duration::from_millis(120)).unwrap();
        assert_eq!(batch.len(), 3, "three distinct paths, one batch");
    }

    #[test]
    fn a_steady_trickle_is_not_deferred_forever() {
        // The window runs from the *first* event, not the most recent, so continuous
        // activity still gets flushed on schedule rather than starving the tree.
        let t0 = Instant::now();
        let mut c = Coalescer::new(window());
        c.push("/r/a.rs".into(), t0);
        for i in 1..20 {
            c.push(format!("/r/f{i}.rs").into(), t0 + Duration::from_millis(i * 10));
        }
        assert!(c.take_if_ready(t0 + Duration::from_millis(101)).is_some());
    }

    #[test]
    fn the_window_restarts_for_the_next_batch() {
        let t0 = Instant::now();
        let mut c = Coalescer::new(window());
        c.push("/r/a.rs".into(), t0);
        assert!(c.take_if_ready(t0 + Duration::from_millis(120)).is_some());
        assert!(c.is_empty());

        let t1 = t0 + Duration::from_millis(500);
        c.push("/r/b.rs".into(), t1);
        assert_eq!(c.take_if_ready(t1 + Duration::from_millis(50)), None, "fresh window");
        assert!(c.take_if_ready(t1 + Duration::from_millis(120)).is_some());
    }

    #[test]
    fn an_empty_coalescer_never_reports_ready() {
        let mut c = Coalescer::new(window());
        assert_eq!(c.take_if_ready(Instant::now()), None);
        assert_eq!(c.time_remaining(Instant::now()), None);
    }

    #[test]
    fn batches_are_deterministically_ordered() {
        let t0 = Instant::now();
        let mut a = Coalescer::new(window());
        let mut b = Coalescer::new(window());
        for p in ["/r/c", "/r/a", "/r/b"] {
            a.push(PathBuf::from(p), t0);
        }
        for p in ["/r/b", "/r/c", "/r/a"] {
            b.push(PathBuf::from(p), t0);
        }
        let ready = t0 + Duration::from_millis(120);
        assert_eq!(a.take_if_ready(ready), b.take_if_ready(ready), "arrival order must not matter");
    }

    #[test]
    fn editor_swap_and_backup_files_are_noise() {
        for p in ["/r/.main.rs.swp", "/r/main.rs~", "/r/4913", "/r/.#main.rs", "/r/build.tmp"] {
            assert!(is_editor_noise(Path::new(p)), "{p} should be filtered out");
        }
    }

    #[test]
    fn real_source_files_are_not_noise() {
        for p in ["/r/main.rs", "/r/Cargo.toml", "/r/src/model.rs"] {
            assert!(!is_editor_noise(Path::new(p)), "{p} must reach the tree");
        }
    }

    #[test]
    fn the_noise_only_filter_passes_real_files() {
        let f = RelevanceFilter::noise_only();
        assert!(f.accepts(Path::new("/r/main.rs")));
        assert!(!f.accepts(Path::new("/r/main.rs~")));
    }
}
