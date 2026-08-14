//! Fuzzy file finding and content search with ripgrep fast paths and native fallbacks.
#![forbid(unsafe_code)]

mod fuzzy;
mod literal;
mod protocol;
mod real;
mod service;
mod worker;

pub use fuzzy::rank_files;
pub use literal::literal_matches;
pub use protocol::{parse_rg_json_line, SearchError};
pub use real::RealSearch;
pub use service::{SearchEventSink, SearchResult, SearchService};
pub use worker::{SearchWorker, DEFAULT_TEXT_DEBOUNCE};

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::AtomicBool;
    use std::sync::{mpsc, Arc, Condvar, Mutex};
    use std::time::Duration;

    use termesh_core::{SearchEvent, SearchMode, SearchRequest, SearchRequestId};

    use super::{
        literal_matches, parse_rg_json_line, rank_files, SearchEventSink, SearchResult,
        SearchService, SearchWorker,
    };

    #[test]
    fn filename_and_contiguous_matches_rank_first() {
        let files = vec![
            PathBuf::from("docs/src-notes.md"),
            PathBuf::from("src/search.rs"),
            PathBuf::from("src/service/archive.rs"),
        ];
        assert_eq!(rank_files(&files, "sr")[0], PathBuf::from("src/search.rs"));
    }

    #[test]
    fn equal_scores_have_a_stable_path_tie_break() {
        let files = vec![PathBuf::from("b/foo.rs"), PathBuf::from("a/foo.rs")];
        assert_eq!(
            rank_files(&files, "foo"),
            vec![PathBuf::from("a/foo.rs"), PathBuf::from("b/foo.rs")]
        );
    }

    #[test]
    fn literal_search_is_smart_case_and_reports_one_based_positions() {
        let lower = literal_matches(Path::new("src/lib.rs"), "Alpha alpha\n", "alpha");
        assert_eq!(lower.len(), 2);
        let upper = literal_matches(Path::new("src/lib.rs"), "Alpha alpha\n", "Alpha");
        assert_eq!((upper[0].line, upper[0].column), (Some(1), Some(1)));
        assert_eq!(upper.len(), 1);
    }

    #[test]
    fn rg_json_byte_offsets_become_character_columns() {
        let raw = r#"{"type":"match","data":{"path":{"text":"src/lib.rs"},"lines":{"text":"éneedle\n"},"line_number":7,"submatches":[{"match":{"text":"needle"},"start":2,"end":8}]}}"#;
        let parsed = parse_rg_json_line(Path::new("/repo"), raw).unwrap();
        assert_eq!(parsed[0].path, Path::new("/repo/src/lib.rs"));
        assert_eq!((parsed[0].line, parsed[0].column), (Some(7), Some(2)));
    }

    /// The ripgrep fast path and the native fallback are one user-facing contract
    /// (ADR-0009 §1): the same query must produce the same results whether or not `rg`
    /// is installed. `literal_matches` is also what overlays dirty editor buffers on top
    /// of disk results, so a disagreement would show up *within a single result list* —
    /// N entries for an open file's line, one for a closed file's identical line.
    #[test]
    fn ripgrep_and_the_native_fallback_agree_on_the_same_line() {
        let line = "Alpha alpha and alpha";
        let raw = format!(
            r#"{{"type":"match","data":{{"path":{{"text":"src/lib.rs"}},"lines":{{"text":"{line}\n"}},"line_number":4,"submatches":[{{"match":{{"text":"alpha"}},"start":6,"end":11}},{{"match":{{"text":"alpha"}},"start":16,"end":21}}]}}}}"#
        );

        let via_ripgrep = parse_rg_json_line(Path::new("/repo"), &raw).unwrap();
        let via_fallback = literal_matches(Path::new("/repo/src/lib.rs"), line, "alpha");

        // Smart-case: the lowercase query matches "Alpha" too, so the fallback finds a
        // hit ripgrep would also have reported. Compare the shared suffix positions.
        assert_eq!(via_fallback.len(), 3, "fallback finds Alpha, alpha, alpha");
        assert_eq!(
            via_ripgrep.iter().map(|found| found.column).collect::<Vec<_>>(),
            [Some(7), Some(17)]
        );
        assert_eq!(
            via_fallback.iter().skip(1).map(|found| found.column).collect::<Vec<_>>(),
            [Some(7), Some(17)],
            "same columns, one result per occurrence, from either path"
        );
        assert!(via_ripgrep.iter().all(|found| found.path == Path::new("/repo/src/lib.rs")));
        assert!(via_ripgrep.iter().all(|found| found.text.as_deref() == Some(line)));
        assert!(via_fallback.iter().all(|found| found.text.as_deref() == Some(line)));
    }

    /// The agreement above uses a line with no overlapping hits, which is exactly the
    /// shape that hides a leftmost-first-vs-every-position disagreement. Pin the
    /// overlapping case separately, against real observed `rg` output: searching "aa"
    /// in "aaaa" reports submatches at byte 0 and byte 2 — two results, not three.
    #[test]
    fn ripgrep_and_the_native_fallback_agree_on_overlapping_candidates() {
        let raw = r#"{"type":"match","data":{"path":{"text":"src/lib.rs"},"lines":{"text":"aaaa\n"},"line_number":1,"submatches":[{"match":{"text":"aa"},"start":0,"end":2},{"match":{"text":"aa"},"start":2,"end":4}]}}"#;

        let via_ripgrep = parse_rg_json_line(Path::new("/repo"), raw).unwrap();
        let via_fallback = literal_matches(Path::new("/repo/src/lib.rs"), "aaaa", "aa");

        assert_eq!(
            via_ripgrep.iter().map(|found| found.column).collect::<Vec<_>>(),
            via_fallback.iter().map(|found| found.column).collect::<Vec<_>>(),
        );
        assert_eq!(via_fallback.len(), 2, "non-overlapping, like ripgrep and editor find");
    }

    struct StubSearch;

    impl SearchService for StubSearch {
        fn search(
            &mut self,
            request: &SearchRequest,
            _cancelled: &AtomicBool,
            sink: &SearchEventSink,
        ) -> SearchResult<()> {
            sink(SearchEvent::Batch {
                id: request.id,
                matches: ["src/lib.rs", "src/main.rs"]
                    .into_iter()
                    .map(|path| termesh_core::SearchMatch {
                        path: request.root.join(path),
                        line: None,
                        column: None,
                        text: None,
                    })
                    .collect(),
            });
            Ok(())
        }
    }

    fn files_request(id: SearchRequestId) -> SearchRequest {
        SearchRequest {
            id,
            root: PathBuf::from("/repo"),
            mode: SearchMode::Files,
            query: String::new(),
            limit: 20,
        }
    }

    #[test]
    fn worker_forwards_bounded_batches() {
        let (tx, rx) = mpsc::channel();
        let worker = SearchWorker::spawn_with_debounce(StubSearch, Duration::ZERO, move |event| {
            tx.send(event).unwrap()
        });
        worker.request(files_request(SearchRequestId::new(7)));
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            SearchEvent::Started { id } if id == SearchRequestId::new(7)
        ));
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            SearchEvent::Batch { matches, .. } if matches.len() == 2
        ));
    }

    #[derive(Clone, Default)]
    struct BlockingSearch {
        state: Arc<(Mutex<Vec<SearchRequestId>>, Condvar)>,
    }

    impl SearchService for BlockingSearch {
        fn search(
            &mut self,
            request: &SearchRequest,
            cancelled: &AtomicBool,
            _sink: &SearchEventSink,
        ) -> SearchResult<()> {
            let (lock, ready) = &*self.state;
            lock.lock().unwrap().push(request.id);
            ready.notify_all();
            while !cancelled.load(std::sync::atomic::Ordering::Acquire) {
                std::thread::yield_now();
            }
            Ok(())
        }
    }

    #[test]
    fn a_new_request_cancels_the_old_token() {
        let service = BlockingSearch::default();
        let state = service.state.clone();
        let worker = SearchWorker::spawn_with_debounce(service, Duration::ZERO, |_| {});
        worker.request(files_request(SearchRequestId::new(1)));
        let (lock, ready) = &*state;
        let mut started = lock.lock().unwrap();
        while started.is_empty() {
            started = ready.wait(started).unwrap();
        }
        drop(started);
        worker.request(files_request(SearchRequestId::new(2)));
        let mut started = lock.lock().unwrap();
        while !started.contains(&SearchRequestId::new(2)) {
            started = ready.wait(started).unwrap();
        }
    }
}
