use std::path::Path;

use termesh_core::TaskSpec;

pub(crate) fn python_tasks(root: &Path) -> Vec<TaskSpec> {
    vec![TaskSpec {
        id: "python.pytest".into(),
        label: "pytest: test".into(),
        program: "pytest".into(),
        args: Vec::new(),
        cwd: root.into(),
    }]
}
