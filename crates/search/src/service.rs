use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use termesh_core::{SearchEvent, SearchRequest};

use crate::SearchError;

pub type SearchEventSink = Arc<dyn Fn(SearchEvent) + Send + Sync>;
pub type SearchResult<T> = Result<T, SearchError>;

/// Synchronous search boundary. Callers keep it off the render thread via
/// [`crate::SearchWorker`].
pub trait SearchService: Send + 'static {
    fn search(
        &mut self,
        request: &SearchRequest,
        cancelled: &AtomicBool,
        sink: &SearchEventSink,
    ) -> SearchResult<()>;
}
