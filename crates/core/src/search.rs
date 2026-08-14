//! Protocol-neutral vocabulary shared by search services and the application model.

use std::path::PathBuf;

use crate::SearchRequestId;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchMode {
    Files,
    Text,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchRequest {
    pub id: SearchRequestId,
    pub root: PathBuf,
    pub mode: SearchMode,
    pub query: String,
    pub limit: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchMatch {
    pub path: PathBuf,
    /// One-based line number when this is a text match.
    pub line: Option<usize>,
    /// One-based character column when this is a text match.
    pub column: Option<usize>,
    pub text: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchEvent {
    Started { id: SearchRequestId },
    Batch { id: SearchRequestId, matches: Vec<SearchMatch> },
    Finished { id: SearchRequestId, truncated: bool },
    Cancelled { id: SearchRequestId },
    Failed { id: SearchRequestId, message: String, partial: bool },
}
