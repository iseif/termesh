//! Workspace root and project-type detection (ADR-0005 §6, ARCHITECTURE.md §16 Phase 02).
//!
//! Goes through [`FileSystemService`] like everything else — never `std::fs` — which is
//! also what makes it testable against an in-memory tree.

use std::path::{Path, PathBuf};

use termesh_filesystem::{FileSystemService, FsError};

/// What kind of project a root looks like. Selects the language recipe and the task
/// adapter, and names the project in agent context.
///
/// A root reports *every* kind it matches, not one: a repository holding both a
/// `pom.xml` and a `package.json` is both, and each side starts its own language session
/// on the first document it claims (ADR-0012).
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProjectKind {
    Rust,
    Java,
    Node,
    Python,
    Go,
    #[default]
    Unknown,
}

impl ProjectKind {
    pub fn label(self) -> &'static str {
        match self {
            ProjectKind::Rust => "rust",
            ProjectKind::Java => "java",
            ProjectKind::Node => "node",
            ProjectKind::Python => "python",
            ProjectKind::Go => "go",
            ProjectKind::Unknown => "unknown",
        }
    }
}

/// One label for a detected set — `"rust, node"`.
///
/// A polyglot root that reads as a single language tells the developer we found less
/// than we did, so every surface that names the project kind uses this rather than the
/// primary alone (ADR-0012 §1). An empty set is `"unknown"`, matching a root found by
/// `.git` or no marker at all.
pub fn kind_labels(kinds: &[ProjectKind]) -> String {
    if kinds.is_empty() {
        return ProjectKind::Unknown.label().to_string();
    }
    kinds.iter().map(|kind| kind.label()).collect::<Vec<_>>().join(", ")
}

/// Marker files that identify project types, in priority order. Higher rows win primary
/// status: Java follows the flagship Rust marker but precedes Node so a Java backend with
/// a `package.json` frontend is identified primarily as Java. A directory holding several
/// markers reports all matches while retaining the first as its primary kind.
const PROJECT_MARKERS: &[(&str, ProjectKind)] = &[
    ("Cargo.toml", ProjectKind::Rust),
    ("pom.xml", ProjectKind::Java),
    ("build.gradle", ProjectKind::Java),
    ("build.gradle.kts", ProjectKind::Java),
    // A Gradle multi-project root often declares only `settings.gradle`, leaving the
    // build files to its modules. Without these rows such a root is not a project at
    // all: no language server and no tasks.
    ("settings.gradle", ProjectKind::Java),
    ("settings.gradle.kts", ProjectKind::Java),
    ("go.mod", ProjectKind::Go),
    ("pyproject.toml", ProjectKind::Python),
    ("package.json", ProjectKind::Node),
];

/// `.git` alone marks a root without identifying a project type.
const VCS_MARKER: &str = ".git";

/// A detected project root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceRoot {
    pub path: PathBuf,
    /// The primary kind retained for display and existing single-answer call sites.
    pub kind: ProjectKind,
    /// Every marker found at this root, in marker-priority order (ADR-0012 §1).
    pub kinds: Vec<ProjectKind>,
    /// True when we found a real marker; false when we fell back to the given directory.
    pub detected: bool,
}

impl WorkspaceRoot {
    /// The name shown in the explorer header — the root directory's own name.
    pub fn display_name(&self) -> String {
        self.path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.path.to_string_lossy().into_owned())
    }
}

/// Walk up from `start` looking for a project or VCS marker.
///
/// The nearest ancestor holding a marker wins, so opening `myrepo/src/deep/` lands on
/// `myrepo`. If nothing matches all the way up, we fall back to `start` itself with
/// `detected: false` — opening a bare directory is legitimate, not an error.
pub fn detect_root(fs: &dyn FileSystemService, start: &Path) -> WorkspaceRoot {
    // Resolve first so `..` segments don't confuse the upward walk. A path we cannot
    // canonicalize (missing, unreadable) still gets used verbatim rather than failing.
    let start = fs.canonicalize(start).unwrap_or_else(|_| start.to_path_buf());

    for dir in start.ancestors() {
        let Ok(entries) = fs.read_dir(dir) else {
            // Unreadable ancestor: stop climbing rather than silently skipping past it.
            break;
        };
        let has = |name: &str| entries.iter().any(|e| e.name == name);

        let mut kinds: Vec<_> = PROJECT_MARKERS
            .iter()
            .filter(|(marker, _)| has(marker))
            .map(|(_, kind)| *kind)
            .collect();
        // Java is the first kind with several markers. Keep aliases from duplicating
        // status, recipes, tasks, and agent context while preserving marker priority
        // (ADR-0013 §2); sorting here would change the primary kind contract.
        let mut seen = Vec::new();
        kinds.retain(|kind| {
            if seen.contains(kind) {
                false
            } else {
                seen.push(*kind);
                true
            }
        });
        if let Some(kind) = kinds.first().copied() {
            return WorkspaceRoot { path: dir.to_path_buf(), kind, kinds, detected: true };
        }
        if has(VCS_MARKER) {
            return WorkspaceRoot {
                path: dir.to_path_buf(),
                kind: ProjectKind::Unknown,
                kinds: Vec::new(),
                detected: true,
            };
        }
    }

    WorkspaceRoot { path: start, kind: ProjectKind::Unknown, kinds: Vec::new(), detected: false }
}

/// Detect the project type of one directory without walking up.
pub fn project_kind_of(fs: &dyn FileSystemService, dir: &Path) -> Result<ProjectKind, FsError> {
    let entries = fs.read_dir(dir)?;
    Ok(PROJECT_MARKERS
        .iter()
        .find(|(marker, _)| entries.iter().any(|e| e.name == *marker))
        .map(|(_, kind)| *kind)
        .unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use termesh_test_support::FakeFileSystem;

    #[test]
    fn climbs_to_the_nearest_marker() {
        let fs = FakeFileSystem::with_paths(&["/repo/Cargo.toml", "/repo/src/deep/mod.rs"]);
        let root = detect_root(&fs, Path::new("/repo/src/deep"));
        assert_eq!(root.path, Path::new("/repo"));
        assert_eq!(root.kind, ProjectKind::Rust);
        assert!(root.detected);
    }

    #[test]
    fn nearest_root_wins_over_an_outer_one() {
        // A crate nested inside a larger repo resolves to the crate, not the repo.
        let fs = FakeFileSystem::with_paths(&[
            "/repo/.git/config",
            "/repo/crates/inner/Cargo.toml",
            "/repo/crates/inner/src/lib.rs",
        ]);
        let root = detect_root(&fs, Path::new("/repo/crates/inner/src"));
        assert_eq!(root.path, Path::new("/repo/crates/inner"));
    }

    #[test]
    fn git_alone_marks_a_root_without_a_project_kind() {
        let fs = FakeFileSystem::with_paths(&["/repo/.git/config", "/repo/notes.txt"]);
        let root = detect_root(&fs, Path::new("/repo"));
        assert_eq!(root.path, Path::new("/repo"));
        assert_eq!(root.kind, ProjectKind::Unknown);
        assert!(root.detected);
    }

    #[test]
    fn a_bare_directory_is_still_a_usable_root() {
        let fs = FakeFileSystem::with_paths(&["/scratch/a.txt"]);
        let root = detect_root(&fs, Path::new("/scratch"));
        assert_eq!(root.path, Path::new("/scratch"));
        assert!(!root.detected, "fallback must be distinguishable from a real detection");
    }

    #[test]
    fn parent_segments_are_resolved_before_climbing() {
        let fs = FakeFileSystem::with_paths(&["/repo/Cargo.toml", "/repo/src/lib.rs"]);
        let root = detect_root(&fs, Path::new("/repo/src/../src"));
        assert_eq!(root.path, Path::new("/repo"));
    }

    #[test]
    fn project_markers_take_priority_over_each_other_deterministically() {
        let fs = FakeFileSystem::with_paths(&["/p/Cargo.toml", "/p/package.json"]);
        assert_eq!(project_kind_of(&fs, Path::new("/p")).unwrap(), ProjectKind::Rust);
    }

    #[test]
    fn a_root_with_several_markers_reports_all_of_them() {
        let fs = FakeFileSystem::with_paths(&[
            "/repo/Cargo.toml",
            "/repo/package.json",
            "/repo/pyproject.toml",
        ]);
        let root = detect_root(&fs, Path::new("/repo"));

        assert_eq!(
            root.kinds,
            vec![ProjectKind::Rust, ProjectKind::Python, ProjectKind::Node],
            "marker priority order, not directory order"
        );
        assert_eq!(root.kind, ProjectKind::Rust, "the primary is still the first match");
    }

    #[test]
    fn a_single_marker_root_is_unchanged() {
        let fs = FakeFileSystem::with_paths(&["/repo/go.mod"]);
        let root = detect_root(&fs, Path::new("/repo"));

        assert_eq!(root.kind, ProjectKind::Go);
        assert_eq!(root.kinds, vec![ProjectKind::Go]);
    }

    #[test]
    fn each_java_marker_maps_to_java() {
        for marker in [
            "pom.xml",
            "build.gradle",
            "build.gradle.kts",
            "settings.gradle",
            "settings.gradle.kts",
        ] {
            let fs = FakeFileSystem::with_paths(&[&format!("/repo/{marker}")]);
            assert_eq!(detect_root(&fs, Path::new("/repo")).kind, ProjectKind::Java, "{marker}");
        }
    }

    #[test]
    fn a_repository_with_several_java_markers_reports_java_once() {
        // Java is the first kind with more than one marker. Every existing kind has
        // exactly one, so nothing has ever guarded against duplicates: without a dedup
        // this reports [Java, Java], the status bar reads "(java, java)", and two
        // recipes claim `.java` while only the first can ever start (ADR-0013 §2).
        let fs = FakeFileSystem::with_paths(&[
            "/repo/pom.xml",
            "/repo/build.gradle",
            "/repo/build.gradle.kts",
        ]);
        let root = detect_root(&fs, Path::new("/repo"));
        assert_eq!(root.kinds, vec![ProjectKind::Java]);
    }

    #[test]
    fn a_java_and_node_repository_reports_both_once_each() {
        let fs = FakeFileSystem::with_paths(&[
            "/repo/pom.xml",
            "/repo/build.gradle",
            "/repo/package.json",
        ]);
        let root = detect_root(&fs, Path::new("/repo"));
        assert_eq!(root.kinds, vec![ProjectKind::Java, ProjectKind::Node]);
        assert_eq!(root.kind, ProjectKind::Java, "marker priority still picks the primary");
    }

    #[test]
    fn the_status_bar_label_lists_java_once() {
        assert_eq!(kind_labels(&[ProjectKind::Java]), "java");
    }

    #[test]
    fn git_alone_still_reports_no_project_kind() {
        let fs = FakeFileSystem::with_paths(&["/repo/.git/HEAD"]);
        let root = detect_root(&fs, Path::new("/repo"));

        assert_eq!(root.kind, ProjectKind::Unknown);
        assert!(root.kinds.is_empty(), "an unknown kind is an empty set, not [Unknown]");
    }

    #[test]
    fn markers_below_the_root_are_not_detected() {
        // Root-level scan on purpose (ADR-0012 §1). Finding nested projects is monorepo
        // support and is out of scope for this phase.
        let fs = FakeFileSystem::with_paths(&["/repo/Cargo.toml", "/repo/web/package.json"]);
        let root = detect_root(&fs, Path::new("/repo"));

        assert_eq!(root.kinds, vec![ProjectKind::Rust]);
    }

    #[test]
    fn each_marker_maps_to_its_kind() {
        for (marker, expected) in
            [("go.mod", ProjectKind::Go), ("pyproject.toml", ProjectKind::Python)]
        {
            let fs = FakeFileSystem::new();
            fs.add_file(format!("/p/{marker}"), b"");
            assert_eq!(project_kind_of(&fs, Path::new("/p")).unwrap(), expected);
        }
    }

    #[test]
    fn display_name_is_the_root_directory_name() {
        let root = WorkspaceRoot {
            path: PathBuf::from("/home/me/myproject"),
            kind: ProjectKind::Rust,
            kinds: vec![ProjectKind::Rust],
            detected: true,
        };
        assert_eq!(root.display_name(), "myproject");
    }
}
