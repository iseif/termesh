use std::path::Path;

use termesh_core::TaskSpec;
use termesh_filesystem::FileSystemService;

const MAVEN_GOALS: &[&str] = &["clean", "compile", "test", "package", "verify"];
const GRADLE_TASKS: &[&str] = &["build", "test", "clean", "check", "assemble"];
/// Kept in step with the Gradle rows of `PROJECT_MARKERS` and with the reload list in
/// `model::is_java_build_file` — a file that identifies a Gradle project must also earn
/// it tasks.
const GRADLE_BUILD_FILES: &[&str] =
    &["build.gradle", "build.gradle.kts", "settings.gradle", "settings.gradle.kts"];

pub(crate) fn java_tasks(fs: &dyn FileSystemService, root: &Path) -> Vec<TaskSpec> {
    java_tasks_for_platform(fs, root, cfg!(windows))
}

fn java_tasks_for_platform(
    fs: &dyn FileSystemService,
    root: &Path,
    windows: bool,
) -> Vec<TaskSpec> {
    let mut tasks = Vec::new();

    if exists(fs, &root.join("pom.xml")) {
        let wrapper = if windows { "mvnw.cmd" } else { "mvnw" };
        let program = wrapper_or_tool(fs, root, wrapper, "mvn");
        tasks.extend(MAVEN_GOALS.iter().map(|goal| task("maven", goal, &program, root)));
    }

    // `settings.gradle` counts: a multi-project root often declares only that, and its
    // conventional tasks run the whole build from there just the same.
    if GRADLE_BUILD_FILES.iter().any(|name| exists(fs, &root.join(name))) {
        let wrapper = if windows { "gradlew.bat" } else { "gradlew" };
        let program = wrapper_or_tool(fs, root, wrapper, "gradle");
        tasks.extend(GRADLE_TASKS.iter().map(|name| task("gradle", name, &program, root)));
    }

    tasks
}

fn exists(fs: &dyn FileSystemService, path: &Path) -> bool {
    fs.read_file(path).is_ok()
}

fn wrapper_or_tool(fs: &dyn FileSystemService, root: &Path, wrapper: &str, tool: &str) -> String {
    let path = root.join(wrapper);
    if exists(fs, &path) {
        path.to_string_lossy().into_owned()
    } else {
        tool.to_string()
    }
}

fn task(adapter: &str, name: &str, program: &str, root: &Path) -> TaskSpec {
    // Root cwd intentionally makes Maven reactor and Gradle multi-project builds run
    // every module. That is correct and potentially slow; per-module catalogs are out
    // of scope for Phase 09 (ADR-0013 Consequences).
    TaskSpec {
        id: format!("{adapter}.{name}"),
        label: format!("{adapter}: {name}"),
        program: program.to_string(),
        args: vec![name.to_string()],
        cwd: root.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AdapterTaskService, TaskService};
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};
    use termesh_filesystem::{DirEntryInfo, FileSystemService, FsError, FsResult};
    use termesh_workspace::{ProjectKind, WorkspaceRoot};

    #[derive(Default)]
    struct TestFileSystem {
        files: BTreeMap<PathBuf, Vec<u8>>,
    }

    impl TestFileSystem {
        fn with_paths(paths: &[&str]) -> Self {
            let mut fs = Self::default();
            for path in paths {
                fs.files.insert(PathBuf::from(path), Vec::new());
            }
            fs
        }
    }

    impl FileSystemService for TestFileSystem {
        fn read_file(&self, path: &Path) -> FsResult<Vec<u8>> {
            self.files.get(path).cloned().ok_or_else(|| FsError::NotFound(path.into()))
        }

        fn read_dir(&self, _path: &Path) -> FsResult<Vec<DirEntryInfo>> {
            panic!("Java task discovery does not list directories")
        }

        fn create_file(&self, _path: &Path) -> FsResult<()> {
            panic!("Java task discovery is read-only")
        }

        fn write_file(&self, _path: &Path, _contents: &[u8]) -> FsResult<()> {
            panic!("Java task discovery is read-only")
        }

        fn create_dir(&self, _path: &Path) -> FsResult<()> {
            panic!("Java task discovery is read-only")
        }

        fn rename(&self, _from: &Path, _to: &Path) -> FsResult<()> {
            panic!("Java task discovery is read-only")
        }

        fn remove_file(&self, _path: &Path) -> FsResult<()> {
            panic!("Java task discovery is read-only")
        }

        fn remove_dir_all(&self, _path: &Path) -> FsResult<()> {
            panic!("Java task discovery is read-only")
        }

        fn canonicalize(&self, path: &Path) -> FsResult<PathBuf> {
            Ok(path.into())
        }
    }

    fn java_and_rust_root() -> WorkspaceRoot {
        WorkspaceRoot {
            path: "/p".into(),
            kind: ProjectKind::Rust,
            kinds: vec![ProjectKind::Rust, ProjectKind::Java],
            detected: true,
        }
    }

    #[test]
    fn a_maven_project_offers_conventional_goals() {
        let fs = TestFileSystem::with_paths(&["/p/pom.xml"]);
        let tasks = java_tasks(&fs, Path::new("/p"));
        assert_eq!(tasks[0].program, "mvn");
        let ids: Vec<_> = tasks.iter().map(|task| task.id.as_str()).collect();
        assert!(ids.contains(&"maven.test"), "{ids:?}");
        assert!(ids.contains(&"maven.package"), "{ids:?}");
    }

    #[test]
    fn a_gradle_project_offers_conventional_tasks() {
        // `settings.gradle` included: a multi-project root frequently declares only
        // that, and it identifies a Gradle build exactly as a build file does.
        for marker in ["build.gradle", "build.gradle.kts", "settings.gradle", "settings.gradle.kts"]
        {
            let fs = TestFileSystem::with_paths(&[&format!("/p/{marker}")]);
            let tasks = java_tasks(&fs, Path::new("/p"));
            assert!(tasks.iter().any(|task| task.id == "gradle.build"), "{marker}");
        }
    }

    /// The workspace-relative wrapper path as the production code builds it: joined, so the
    /// separator is the host's.
    fn wrapper_path(name: &str) -> String {
        Path::new("/p").join(name).to_string_lossy().into_owned()
    }

    #[test]
    fn the_wrapper_is_preferred_and_resolved_absolutely() {
        let fs = TestFileSystem::with_paths(&["/p/pom.xml", "/p/mvnw"]);
        let tasks = java_tasks_for_platform(&fs, Path::new("/p"), false);
        // Built with `join`, not spelled out: the wrapper path uses the host's separator,
        // so a literal "/p/mvnw" compares unequal to the "/p\\mvnw" produced on Windows.
        assert_eq!(tasks[0].program, wrapper_path("mvnw"));
    }

    #[test]
    fn a_project_with_both_build_systems_offers_both() {
        let fs = TestFileSystem::with_paths(&["/p/pom.xml", "/p/build.gradle"]);
        let tasks = java_tasks(&fs, Path::new("/p"));
        assert!(tasks.iter().any(|task| task.id.starts_with("maven.")));
        assert!(tasks.iter().any(|task| task.id.starts_with("gradle.")));
    }

    #[test]
    fn windows_wrapper_names_are_selected_on_windows() {
        let fs = TestFileSystem::with_paths(&[
            "/p/pom.xml",
            "/p/mvnw",
            "/p/mvnw.cmd",
            "/p/build.gradle",
            "/p/gradlew",
            "/p/gradlew.bat",
        ]);
        let tasks = java_tasks_for_platform(&fs, Path::new("/p"), true);
        assert!(tasks
            .iter()
            .filter(|task| task.id.starts_with("maven."))
            .all(|task| task.program == wrapper_path("mvnw.cmd")));
        assert!(tasks
            .iter()
            .filter(|task| task.id.starts_with("gradle."))
            .all(|task| task.program == wrapper_path("gradlew.bat")));
    }

    #[test]
    fn java_tasks_join_the_catalog_beside_other_languages() {
        let fs = TestFileSystem::with_paths(&["/p/pom.xml", "/p/Cargo.toml"]);
        let tasks = AdapterTaskService.catalog(&java_and_rust_root(), &fs);
        assert!(tasks.iter().any(|task| task.program == "cargo"));
        assert!(tasks.iter().any(|task| task.id.starts_with("maven.")));
    }
}
