//! Deterministic search service for model and rendering tests.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use termesh_core::{SearchEvent, SearchMode, SearchRequest};
use termesh_search::{SearchEventSink, SearchResult, SearchService};

struct Script {
    mode: SearchMode,
    query: String,
    events: Vec<SearchEvent>,
}

#[derive(Default)]
struct State {
    scripts: VecDeque<Script>,
    requests: Vec<SearchRequest>,
}

type Shared = Arc<Mutex<State>>;

#[derive(Clone, Default)]
pub struct ScriptedSearch {
    shared: Shared,
}

#[derive(Clone)]
pub struct ScriptedSearchControl {
    shared: Shared,
}

impl ScriptedSearch {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn control(&self) -> ScriptedSearchControl {
        ScriptedSearchControl { shared: self.shared.clone() }
    }

    pub fn with_script(
        self,
        mode: SearchMode,
        query: impl Into<String>,
        events: Vec<SearchEvent>,
    ) -> Self {
        self.queue_script(mode, query, events);
        self
    }

    pub fn queue_script(
        &self,
        mode: SearchMode,
        query: impl Into<String>,
        events: Vec<SearchEvent>,
    ) {
        self.shared.lock().expect("scripted search state poisoned").scripts.push_back(Script {
            mode,
            query: query.into(),
            events,
        });
    }
}

impl ScriptedSearchControl {
    pub fn requests(&self) -> Vec<SearchRequest> {
        self.shared.lock().expect("scripted search state poisoned").requests.clone()
    }
}

impl SearchService for ScriptedSearch {
    fn search(
        &mut self,
        request: &SearchRequest,
        cancelled: &AtomicBool,
        sink: &SearchEventSink,
    ) -> SearchResult<()> {
        let events = {
            let mut state = self.shared.lock().expect("scripted search state poisoned");
            state.requests.push(request.clone());
            let script = state
                .scripts
                .iter()
                .position(|script| script.mode == request.mode && script.query == request.query)
                .and_then(|index| state.scripts.remove(index));
            script.map(|script| script.events).unwrap_or_default()
        };

        for event in events {
            if cancelled.load(Ordering::Acquire) {
                break;
            }
            sink(event);
        }
        Ok(())
    }
}
