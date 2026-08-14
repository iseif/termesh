use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use termesh_core::{PreviewRequestId, SearchMatch, SearchMode, SearchRequestId};
use termesh_ui::Pane;

const FILE_CANDIDATE_LIMIT: usize = 20_000;
const VISIBLE_LIMIT: usize = 200;
const TEXT_MATCH_LIMIT: usize = 1_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchStatus {
    Waiting,
    Searching,
    Finished,
    Cancelled,
    Failed(String),
}

/// Model-owned state for quick-open and, in the next slice, workspace text search.
pub struct SearchOverlay {
    pub mode: SearchMode,
    pub query: String,
    pub request: SearchRequestId,
    pub root: PathBuf,
    matches: Vec<SearchMatch>,
    visible: Vec<usize>,
    pub selected: usize,
    pub status: SearchStatus,
    pub truncated: bool,
    pub previous_focus: Pane,
    excluded_paths: HashSet<PathBuf>,
    preview_request: Option<PreviewRequestId>,
    preview_path: Option<PathBuf>,
    preview_line: usize,
    preview_start_line: usize,
    preview_text: Option<String>,
}

impl SearchOverlay {
    pub fn files(request: SearchRequestId, root: PathBuf, previous_focus: Pane) -> Self {
        Self {
            mode: SearchMode::Files,
            query: String::new(),
            request,
            root,
            matches: Vec::new(),
            visible: Vec::new(),
            selected: 0,
            status: SearchStatus::Waiting,
            truncated: false,
            previous_focus,
            excluded_paths: HashSet::new(),
            preview_request: None,
            preview_path: None,
            preview_line: 1,
            preview_start_line: 1,
            preview_text: None,
        }
    }

    pub fn text(request: SearchRequestId, root: PathBuf, previous_focus: Pane) -> Self {
        let mut search = Self::files(request, root, previous_focus);
        search.mode = SearchMode::Text;
        search
    }

    pub fn push_char(&mut self, value: char) {
        self.query.push(value);
        self.refilter();
    }

    pub fn pop_char(&mut self) {
        self.query.pop();
        self.refilter();
    }

    pub fn move_down(&mut self) {
        if !self.visible.is_empty() {
            self.selected = (self.selected + 1) % self.visible.len();
        }
    }

    pub fn move_up(&mut self) {
        if !self.visible.is_empty() {
            self.selected = (self.selected + self.visible.len() - 1) % self.visible.len();
        }
    }

    pub fn selected(&self) -> Option<&SearchMatch> {
        self.visible.get(self.selected).and_then(|index| self.matches.get(*index))
    }

    pub fn replace_results(&mut self, matches: Vec<SearchMatch>) {
        let limit = self.result_limit();
        self.matches = matches.into_iter().take(limit).collect();
        self.clear_preview();
        self.refilter();
    }

    pub fn append_results(&mut self, matches: Vec<SearchMatch>) {
        let remaining = self.result_limit().saturating_sub(self.matches.len());
        self.matches.extend(
            matches
                .into_iter()
                .filter(|found| !self.excluded_paths.contains(&found.path))
                .take(remaining),
        );
        self.refilter();
    }

    pub fn refilter(&mut self) {
        if self.mode == SearchMode::Text {
            self.matches.sort_by(|left, right| {
                left.path
                    .cmp(&right.path)
                    .then(left.line.cmp(&right.line))
                    .then(left.column.cmp(&right.column))
            });
            self.visible = (0..self.matches.len().min(VISIBLE_LIMIT)).collect();
            if self.selected >= self.visible.len() {
                self.selected = self.visible.len().saturating_sub(1);
            }
            return;
        }
        let paths: Vec<PathBuf> = self.matches.iter().map(|found| found.path.clone()).collect();
        let ranked = termesh_search::rank_files(&paths, &self.query);
        let by_path: HashMap<&Path, usize> = self
            .matches
            .iter()
            .enumerate()
            .map(|(index, found)| (found.path.as_path(), index))
            .collect();
        self.visible = ranked
            .iter()
            .filter_map(|path| by_path.get(path.as_path()).copied())
            .take(VISIBLE_LIMIT)
            .collect();
        if self.selected >= self.visible.len() {
            self.selected = self.visible.len().saturating_sub(1);
        }
    }

    pub fn visible_matches(&self) -> Vec<&SearchMatch> {
        self.visible.iter().filter_map(|index| self.matches.get(*index)).collect()
    }

    pub fn view_items(&self) -> Vec<String> {
        self.visible_matches()
            .into_iter()
            .map(|found| {
                let path = found
                    .path
                    .strip_prefix(&self.root)
                    .unwrap_or(&found.path)
                    .to_string_lossy()
                    .into_owned();
                match (found.line, found.column, found.text.as_deref()) {
                    (Some(line), Some(column), Some(text)) => {
                        format!("{path}:{line}:{column}  {}", text.trim())
                    }
                    _ => path,
                }
            })
            .collect()
    }

    pub fn set_request(&mut self, request: SearchRequestId) {
        self.request = request;
    }

    pub fn set_live_results(&mut self, matches: Vec<SearchMatch>, paths: HashSet<PathBuf>) {
        self.excluded_paths = paths;
        self.replace_results(matches);
    }

    pub fn preview_request(&self) -> Option<PreviewRequestId> {
        self.preview_request
    }

    pub fn preview_key(&self) -> Option<(&Path, usize)> {
        Some((self.preview_path.as_deref()?, self.preview_line))
    }

    pub fn await_preview(&mut self, request: PreviewRequestId, path: PathBuf, line: usize) {
        self.preview_request = Some(request);
        self.preview_path = Some(path);
        self.preview_line = line;
        self.preview_text = None;
    }

    pub fn set_preview(&mut self, path: PathBuf, line: usize, start_line: usize, text: String) {
        self.preview_request = None;
        self.preview_path = Some(path);
        self.preview_line = line;
        self.preview_start_line = start_line;
        self.preview_text = Some(text);
    }

    pub fn preview_text(&self) -> Option<&str> {
        self.preview_text.as_deref()
    }

    pub fn preview_start_line(&self) -> usize {
        self.preview_start_line
    }

    pub fn clear_preview(&mut self) {
        self.preview_request = None;
        self.preview_path = None;
        self.preview_line = 1;
        self.preview_text = None;
        self.preview_start_line = 1;
    }

    fn result_limit(&self) -> usize {
        match self.mode {
            SearchMode::Files => FILE_CANDIDATE_LIMIT,
            SearchMode::Text => TEXT_MATCH_LIMIT,
        }
    }

    pub fn status_text(&self) -> String {
        match &self.status {
            SearchStatus::Waiting if self.query.is_empty() => "type to search".into(),
            SearchStatus::Waiting | SearchStatus::Searching => "searching…".into(),
            SearchStatus::Finished => {
                let suffix = if self.truncated { " · more results omitted" } else { "" };
                format!("{} result(s){suffix}", self.visible.len())
            }
            SearchStatus::Cancelled => "cancelled".into(),
            SearchStatus::Failed(message) => message.clone(),
        }
    }
}
