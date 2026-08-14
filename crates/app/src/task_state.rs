use std::path::PathBuf;

use termesh_core::{
    DiagnosticOrigin, DiagnosticSeverity, Problem, ProblemSeverity, TaskOrigin, TaskRunId,
    TaskSpec, TaskStatus, TerminalId,
};
use termesh_tasks::TaskOutputDecoder;

pub struct TaskPicker {
    items: Vec<TaskSpec>,
    pub selected: usize,
}

impl TaskPicker {
    pub fn new(items: Vec<TaskSpec>) -> Self {
        Self { items, selected: 0 }
    }
    pub fn items(&self) -> &[TaskSpec] {
        &self.items
    }
    pub fn selected(&self) -> Option<&TaskSpec> {
        self.items.get(self.selected)
    }
    pub fn move_down(&mut self) {
        if !self.items.is_empty() {
            self.selected = (self.selected + 1) % self.items.len();
        }
    }
    pub fn move_up(&mut self) {
        if !self.items.is_empty() {
            self.selected = (self.selected + self.items.len() - 1) % self.items.len();
        }
    }
}

pub struct TaskRun {
    pub id: TaskRunId,
    pub spec: TaskSpec,
    pub terminal: TerminalId,
    pub origin: TaskOrigin,
    pub status: TaskStatus,
    pub problems: Vec<Problem>,
    pub cancel_requested: bool,
    pub(crate) decoder: Option<Box<dyn TaskOutputDecoder>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProblemRow {
    pub path: PathBuf,
    /// Human-facing, one-based coordinates.
    pub line: usize,
    pub column: usize,
    pub severity: DiagnosticSeverity,
    pub origin: DiagnosticOrigin,
    pub source: String,
    pub message: String,
}

impl ProblemRow {
    pub fn navigation_problem(&self) -> Problem {
        Problem {
            path: self.path.clone(),
            line: self.line,
            column: self.column,
            severity: if self.severity == DiagnosticSeverity::Error {
                ProblemSeverity::Error
            } else {
                ProblemSeverity::Warning
            },
            message: self.message.clone(),
        }
    }
}

pub struct ProblemsOverlay {
    items: Vec<ProblemRow>,
    pub selected: usize,
}

impl ProblemsOverlay {
    pub fn new(items: Vec<ProblemRow>, selected: usize) -> Self {
        let selected = selected.min(items.len().saturating_sub(1));
        Self { items, selected }
    }

    pub fn items(&self) -> &[ProblemRow] {
        &self.items
    }

    pub fn selected(&self) -> Option<&ProblemRow> {
        self.items.get(self.selected)
    }

    pub fn move_down(&mut self) {
        if !self.items.is_empty() {
            self.selected = (self.selected + 1) % self.items.len();
        }
    }

    pub fn move_up(&mut self) {
        if !self.items.is_empty() {
            self.selected = (self.selected + self.items.len() - 1) % self.items.len();
        }
    }
}
