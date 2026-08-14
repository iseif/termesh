use termesh_core::{Problem, TaskSpec};
use termesh_filesystem::FileSystemService;
use termesh_workspace::WorkspaceRoot;

#[derive(Debug, Default, PartialEq, Eq)]
pub struct DecodedTaskOutput {
    pub display: Vec<u8>,
    pub problems: Vec<Problem>,
}

pub trait TaskOutputDecoder: Send {
    fn push(&mut self, bytes: &[u8]) -> DecodedTaskOutput;
    fn finish(&mut self) -> DecodedTaskOutput;
}

pub trait TaskService: Send + Sync {
    fn catalog(&self, root: &WorkspaceRoot, fs: &dyn FileSystemService) -> Vec<TaskSpec>;
    fn decoder(&self, task: &TaskSpec) -> Option<Box<dyn TaskOutputDecoder>>;
}
