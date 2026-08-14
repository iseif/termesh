//! Deterministic language-neutral task catalog and decoder.

use std::sync::Mutex;

use termesh_core::{Problem, TaskSpec};
use termesh_filesystem::FileSystemService;
use termesh_tasks::{DecodedTaskOutput, TaskOutputDecoder, TaskService};
use termesh_workspace::WorkspaceRoot;

pub struct FakeTaskService {
    catalog: Vec<TaskSpec>,
    problems: Vec<Problem>,
    requested: Mutex<Vec<String>>,
}

impl FakeTaskService {
    pub fn new(catalog: Vec<TaskSpec>) -> Self {
        Self { catalog, problems: Vec::new(), requested: Mutex::new(Vec::new()) }
    }

    pub fn with_problems(mut self, problems: Vec<Problem>) -> Self {
        self.problems = problems;
        self
    }

    pub fn requested(&self) -> Vec<String> {
        self.requested.lock().expect("fake task state poisoned").clone()
    }
}

impl TaskService for FakeTaskService {
    fn catalog(&self, _root: &WorkspaceRoot, _fs: &dyn FileSystemService) -> Vec<TaskSpec> {
        self.catalog.clone()
    }

    fn decoder(&self, task: &TaskSpec) -> Option<Box<dyn TaskOutputDecoder>> {
        if !self.catalog.iter().any(|candidate| candidate.id == task.id) {
            return None;
        }
        self.requested.lock().expect("fake task state poisoned").push(task.id.clone());
        Some(Box::new(FakeDecoder { problems: self.problems.clone(), emitted: false }))
    }
}

struct FakeDecoder {
    problems: Vec<Problem>,
    emitted: bool,
}

impl TaskOutputDecoder for FakeDecoder {
    fn push(&mut self, bytes: &[u8]) -> DecodedTaskOutput {
        let problems = if self.emitted {
            Vec::new()
        } else {
            self.emitted = true;
            self.problems.clone()
        };
        DecodedTaskOutput { display: bytes.to_vec(), problems }
    }

    fn finish(&mut self) -> DecodedTaskOutput {
        DecodedTaskOutput::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FakeFileSystem;
    use std::path::PathBuf;
    use termesh_workspace::ProjectKind;

    #[test]
    fn fake_records_decoder_requests_and_passes_output_through() {
        let task = TaskSpec {
            id: "demo".into(),
            label: "Demo".into(),
            program: "demo".into(),
            args: Vec::new(),
            cwd: PathBuf::from("/p"),
        };
        let fake = FakeTaskService::new(vec![task.clone()]);
        let root = WorkspaceRoot {
            path: "/p".into(),
            kind: ProjectKind::Unknown,
            kinds: Vec::new(),
            detected: true,
        };
        assert_eq!(fake.catalog(&root, &FakeFileSystem::new()), vec![task.clone()]);
        let mut decoder = fake.decoder(&task).unwrap();
        assert_eq!(decoder.push(b"hello").display, b"hello");
        assert_eq!(fake.requested(), vec!["demo"]);
    }
}
