use std::path::Path;

use termesh_core::TaskSpec;
use termesh_filesystem::FileSystemService;

const MAX_SCRIPTS: usize = 128;

pub(crate) fn node_tasks(fs: &dyn FileSystemService, root: &Path) -> Vec<TaskSpec> {
    let bytes = match fs.read_file(&root.join("package.json")) {
        Ok(bytes) => bytes,
        Err(_) => return Vec::new(),
    };
    let manifest: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(manifest) => manifest,
        Err(_) => return Vec::new(),
    };
    let Some(scripts) = manifest.get("scripts").and_then(serde_json::Value::as_object) else {
        return Vec::new();
    };
    let manager = package_manager(fs, root);
    let mut names: Vec<_> = scripts
        .iter()
        .filter_map(|(name, command)| command.is_string().then_some(name.as_str()))
        .collect();
    names.sort_unstable();
    names.truncate(MAX_SCRIPTS);
    names
        .into_iter()
        .map(|name| TaskSpec {
            id: format!("npm.{name}"),
            label: format!("{manager}: {name}"),
            program: manager.to_string(),
            args: vec!["run".into(), name.into()],
            cwd: root.into(),
        })
        .collect()
}

fn package_manager(fs: &dyn FileSystemService, root: &Path) -> &'static str {
    [
        ("pnpm-lock.yaml", "pnpm"),
        ("yarn.lock", "yarn"),
        ("bun.lockb", "bun"),
        ("package-lock.json", "npm"),
    ]
    .into_iter()
    .find_map(|(lockfile, manager)| fs.read_file(&root.join(lockfile)).is_ok().then_some(manager))
    .unwrap_or("npm")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TaskService;
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};
    use termesh_filesystem::{DirEntryInfo, FsError, FsResult};
    use termesh_workspace::{ProjectKind, WorkspaceRoot};

    #[derive(Default)]
    struct TestFileSystem {
        files: BTreeMap<PathBuf, Vec<u8>>,
    }

    impl TestFileSystem {
        fn add_file(&mut self, path: impl Into<PathBuf>, bytes: &[u8]) {
            self.files.insert(path.into(), bytes.to_vec());
        }
    }

    impl FileSystemService for TestFileSystem {
        fn read_file(&self, path: &Path) -> FsResult<Vec<u8>> {
            self.files.get(path).cloned().ok_or_else(|| FsError::NotFound(path.into()))
        }

        fn read_dir(&self, _path: &Path) -> FsResult<Vec<DirEntryInfo>> {
            panic!("node task discovery does not list directories")
        }

        fn create_file(&self, _path: &Path) -> FsResult<()> {
            panic!("node task discovery is read-only")
        }

        fn write_file(&self, _path: &Path, _contents: &[u8]) -> FsResult<()> {
            panic!("node task discovery is read-only")
        }

        fn create_dir(&self, _path: &Path) -> FsResult<()> {
            panic!("node task discovery is read-only")
        }

        fn rename(&self, _from: &Path, _to: &Path) -> FsResult<()> {
            panic!("node task discovery is read-only")
        }

        fn remove_file(&self, _path: &Path) -> FsResult<()> {
            panic!("node task discovery is read-only")
        }

        fn remove_dir_all(&self, _path: &Path) -> FsResult<()> {
            panic!("node task discovery is read-only")
        }

        fn canonicalize(&self, path: &Path) -> FsResult<PathBuf> {
            Ok(path.into())
        }
    }

    fn polyglot_root() -> WorkspaceRoot {
        WorkspaceRoot {
            path: "/p".into(),
            kind: ProjectKind::Rust,
            kinds: vec![ProjectKind::Rust, ProjectKind::Node],
            detected: true,
        }
    }

    #[test]
    fn scripts_declared_in_package_json_become_tasks() {
        let mut fs = TestFileSystem::default();
        fs.add_file("/p/package.json", br#"{"scripts":{"build":"tsc","test":"vitest"}}"#);
        let tasks = node_tasks(&fs, Path::new("/p"));
        assert_eq!(tasks.len(), 2);
        let build = tasks.iter().find(|task| task.id == "npm.build").unwrap();
        assert_eq!(build.program, "npm");
        assert_eq!(build.args, vec!["run", "build"]);
    }

    #[test]
    fn the_lockfile_chooses_the_package_manager() {
        for (lockfile, program) in [
            ("pnpm-lock.yaml", "pnpm"),
            ("yarn.lock", "yarn"),
            ("bun.lockb", "bun"),
            ("package-lock.json", "npm"),
        ] {
            let mut fs = TestFileSystem::default();
            fs.add_file("/p/package.json", br#"{"scripts":{"build":"tsc"}}"#);
            fs.add_file(format!("/p/{lockfile}"), b"");
            assert_eq!(node_tasks(&fs, Path::new("/p"))[0].program, program, "{lockfile}");
        }
    }

    #[test]
    fn no_lockfile_falls_back_to_npm() {
        let mut fs = TestFileSystem::default();
        fs.add_file("/p/package.json", br#"{"scripts":{"build":"tsc"}}"#);
        assert_eq!(node_tasks(&fs, Path::new("/p"))[0].program, "npm");
    }

    #[test]
    fn a_package_json_without_scripts_yields_no_tasks() {
        let mut fs = TestFileSystem::default();
        fs.add_file("/p/package.json", br#"{"name":"plain"}"#);
        assert!(node_tasks(&fs, Path::new("/p")).is_empty());
    }

    #[test]
    fn malformed_package_json_yields_no_node_tasks_and_keeps_the_others() {
        let mut fs = TestFileSystem::default();
        fs.add_file("/p/Cargo.toml", b"");
        fs.add_file("/p/package.json", b"{ not json");
        let tasks = crate::AdapterTaskService::cargo_only().catalog(&polyglot_root(), &fs);
        assert!(tasks.iter().any(|task| task.program == "cargo"));
        assert!(!tasks.iter().any(|task| task.program == "npm"));
    }

    #[test]
    fn script_names_are_never_interpolated_into_a_shell_string() {
        let mut fs = TestFileSystem::default();
        fs.add_file("/p/package.json", br#"{"scripts":{"a; rm -rf /":"echo"}}"#);
        let tasks = node_tasks(&fs, Path::new("/p"));
        assert_eq!(tasks[0].args, vec!["run", "a; rm -rf /"]);
    }

    #[test]
    fn script_discovery_is_bounded_and_sorted() {
        let scripts = (0..140)
            .rev()
            .map(|index| format!(r#""script-{index:03}":"echo""#))
            .collect::<Vec<_>>()
            .join(",");
        let mut fs = TestFileSystem::default();
        fs.add_file("/p/package.json", format!(r#"{{"scripts":{{{scripts}}}}}"#).as_bytes());

        let tasks = node_tasks(&fs, Path::new("/p"));

        assert_eq!(tasks.len(), MAX_SCRIPTS);
        assert!(tasks.windows(2).all(|pair| pair[0].id < pair[1].id));
    }
}
