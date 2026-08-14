use termesh_core::TaskSpec;
use termesh_filesystem::FileSystemService;
use termesh_workspace::{ProjectKind, WorkspaceRoot};

use crate::{
    java::java_tasks, node::node_tasks, python::python_tasks, CargoOutputDecoder,
    TaskOutputDecoder, TaskService, TextProblemDecoder,
};

const TASKS: &[(&str, &str, &str)] = &[
    ("cargo.check", "Check", "check"),
    ("cargo.build", "Build", "build"),
    ("cargo.test", "Test", "test"),
    ("cargo.clippy", "Clippy", "clippy"),
];

#[derive(Debug, Clone, Copy, Default)]
pub struct AdapterTaskService;

impl AdapterTaskService {
    pub fn cargo_only() -> Self {
        Self
    }
}

impl TaskService for AdapterTaskService {
    fn catalog(&self, root: &WorkspaceRoot, fs: &dyn FileSystemService) -> Vec<TaskSpec> {
        let mut tasks = Vec::new();
        if root.kinds.contains(&ProjectKind::Rust) {
            tasks.extend(TASKS.iter().map(|(id, label, subcommand)| TaskSpec {
                id: (*id).into(),
                label: (*label).into(),
                program: "cargo".into(),
                args: vec![
                    (*subcommand).into(),
                    "--message-format=json-diagnostic-rendered-ansi".into(),
                ],
                cwd: root.path.clone(),
            }));
        }
        if root.kinds.contains(&ProjectKind::Node) {
            tasks.extend(node_tasks(fs, &root.path));
        }
        if root.kinds.contains(&ProjectKind::Python) {
            tasks.extend(python_tasks(&root.path));
        }
        if root.kinds.contains(&ProjectKind::Java) {
            tasks.extend(java_tasks(fs, &root.path));
        }
        tasks
    }

    fn decoder(&self, task: &TaskSpec) -> Option<Box<dyn TaskOutputDecoder>> {
        if TASKS.iter().any(|(id, _, _)| *id == task.id) {
            Some(Box::new(CargoOutputDecoder::new(task.cwd.clone())))
        } else {
            Some(Box::new(TextProblemDecoder::new(task.cwd.clone())))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use termesh_filesystem::RealFileSystem;

    fn root(kind: ProjectKind) -> WorkspaceRoot {
        WorkspaceRoot { path: "/p".into(), kind, kinds: vec![kind], detected: true }
    }

    #[test]
    fn the_cargo_adapter_ignores_the_filesystem_it_is_handed() {
        let service = AdapterTaskService::cargo_only();
        let tasks = service.catalog(&root(ProjectKind::Rust), &RealFileSystem);
        assert_eq!(tasks.len(), 4);
    }

    #[test]
    fn rust_projects_get_exactly_four_curated_tasks() {
        let service = AdapterTaskService::cargo_only();
        let tasks = service.catalog(&root(ProjectKind::Rust), &RealFileSystem);
        assert_eq!(
            tasks.iter().map(|task| task.id.as_str()).collect::<Vec<_>>(),
            ["cargo.check", "cargo.build", "cargo.test", "cargo.clippy"]
        );
        assert!(tasks.iter().all(|task| {
            task.program == "cargo"
                && task.args.last().map(String::as_str)
                    == Some("--message-format=json-diagnostic-rendered-ansi")
                && task.cwd == Path::new("/p")
        }));
    }

    #[test]
    fn non_rust_projects_get_no_cargo_tasks() {
        let service = AdapterTaskService::cargo_only();
        assert!(service.catalog(&root(ProjectKind::Node), &RealFileSystem).is_empty());
    }

    #[test]
    fn a_python_project_offers_a_conventional_test_task() {
        let service = AdapterTaskService::cargo_only();
        let tasks = service.catalog(&root(ProjectKind::Python), &RealFileSystem);
        assert!(tasks.iter().any(|task| task.program == "pytest"));
    }
}
