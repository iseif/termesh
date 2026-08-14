//! The slice of workspace state offered to the agent as context (ADR-0005 §7,
//! ARCHITECTURE.md §9.2).
//!
//! The standing question for anything the workspace learns is *"does the agent get to
//! see this?"*. This is the answer for the file tree: a typed snapshot built by a pure
//! function from the same tree the human is looking at.
//!
//! Building it from `FileTree` rather than from the filesystem is the whole point. The
//! agent gets exactly what the human sees — same ignore rules, same expansion state, no
//! `target/`, no `.git` — because there is only one source of truth. A second traversal
//! would be a second chance to disagree.
//!
//! No `AgentService` exists until Phase 03, so this deliberately stops at the typed
//! value: the serialization format and the point in the ACP turn at which it is attached
//! are that phase's decisions, recorded in its own ADR.

use std::path::{Path, PathBuf};

use termesh_filesystem::FileTree;

use crate::root::{ProjectKind, WorkspaceRoot};

/// One entry in the tree as the agent sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeEntry {
    /// Path relative to the workspace root — absolute paths leak the user's home
    /// directory into agent context for no benefit.
    pub path: PathBuf,
    pub depth: usize,
    pub is_dir: bool,
    /// Directories the user has not opened. Flagged so the agent can tell "this is
    /// empty" from "I have not looked inside", and ask for it if it needs to.
    pub unexplored: bool,
}

/// What the agent is told about the workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceSnapshot {
    pub root: PathBuf,
    pub project_kind: ProjectKind,
    /// Every detected project kind, matching the root's marker-priority order.
    pub project_kinds: Vec<ProjectKind>,
    /// The currently visible tree — loaded, expanded, ignore-filtered.
    pub visible_tree: Vec<TreeEntry>,
    /// What the human currently has selected, relative to the root.
    pub selection: Option<PathBuf>,
}

impl WorkspaceSnapshot {
    /// Build the snapshot from the live tree. Pure — no I/O, so it is snapshot-testable
    /// and cannot drift from what is on screen.
    pub fn build(root: &WorkspaceRoot, tree: &FileTree) -> Self {
        let rows = tree.visible_rows();
        let selected = tree.selected();

        let visible_tree = rows
            .iter()
            // Skip the root row itself; `root` already names it.
            .filter(|r| r.id != tree.root())
            .filter_map(|r| {
                let full = tree.path_of(r.id)?;
                Some(TreeEntry {
                    path: relative_to(&root.path, full),
                    // The root is depth 0, so its children start at 1; re-base to 0.
                    depth: r.depth.saturating_sub(1),
                    is_dir: r.is_expandable,
                    unexplored: r.is_expandable && !r.expanded,
                })
            })
            .collect();

        let selection = tree
            .path_of(selected)
            .filter(|_| selected != tree.root())
            .map(|p| relative_to(&root.path, p));

        Self {
            root: root.path.clone(),
            project_kind: root.kind,
            project_kinds: root.kinds.clone(),
            visible_tree,
            selection,
        }
    }

    /// Number of entries the agent can currently see.
    pub fn len(&self) -> usize {
        self.visible_tree.len()
    }

    pub fn is_empty(&self) -> bool {
        self.visible_tree.is_empty()
    }
}

/// Strip the root prefix, falling back to the full path if it is not underneath.
fn relative_to(root: &Path, path: &Path) -> PathBuf {
    path.strip_prefix(root).unwrap_or(path).to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;
    use termesh_filesystem::{DirEntryInfo, EntryKind};

    fn root() -> WorkspaceRoot {
        WorkspaceRoot {
            path: PathBuf::from("/proj"),
            kind: ProjectKind::Rust,
            kinds: vec![ProjectKind::Rust],
            detected: true,
        }
    }

    fn entry(name: &str, kind: EntryKind) -> DirEntryInfo {
        DirEntryInfo { name: name.into(), path: PathBuf::from("/proj").join(name), kind }
    }

    /// A tree with `/proj` expanded and `src` present but unopened.
    fn tree() -> FileTree {
        let mut t = FileTree::new("/proj", "proj");
        let _ = t.expand(t.root());
        t.set_children(
            t.root(),
            vec![entry("src", EntryKind::Dir), entry("Cargo.toml", EntryKind::File)],
        );
        t
    }

    fn paths(s: &WorkspaceSnapshot) -> Vec<String> {
        s.visible_tree.iter().map(|e| e.path.to_string_lossy().into_owned()).collect()
    }

    #[test]
    fn the_snapshot_carries_the_root_and_project_kind() {
        let s = WorkspaceSnapshot::build(&root(), &tree());
        assert_eq!(s.root, Path::new("/proj"));
        assert_eq!(s.project_kind, ProjectKind::Rust);
        assert_eq!(s.project_kinds, vec![ProjectKind::Rust]);
    }

    #[test]
    fn paths_are_relative_to_the_root() {
        let s = WorkspaceSnapshot::build(&root(), &tree());
        assert_eq!(paths(&s), ["src", "Cargo.toml"]);
        assert!(
            !paths(&s).iter().any(|p| p.starts_with('/')),
            "absolute paths would leak the user's home directory into agent context"
        );
    }

    #[test]
    fn the_root_row_itself_is_not_repeated_as_an_entry() {
        let s = WorkspaceSnapshot::build(&root(), &tree());
        assert!(!paths(&s).contains(&"proj".to_string()));
    }

    #[test]
    fn unopened_directories_are_flagged_as_unexplored() {
        let s = WorkspaceSnapshot::build(&root(), &tree());
        let src = s.visible_tree.iter().find(|e| e.path == Path::new("src")).unwrap();
        assert!(src.is_dir);
        assert!(src.unexplored, "the agent must be able to ask for what it cannot see");

        let toml = s.visible_tree.iter().find(|e| e.path == Path::new("Cargo.toml")).unwrap();
        assert!(!toml.unexplored, "files are never unexplored");
    }

    #[test]
    fn an_expanded_directory_is_not_unexplored_and_its_children_appear() {
        let mut t = tree();
        let src = t.visible_rows()[1].id;
        let _ = t.expand(src);
        t.set_children(
            src,
            vec![DirEntryInfo {
                name: "main.rs".into(),
                path: "/proj/src/main.rs".into(),
                kind: EntryKind::File,
            }],
        );

        let s = WorkspaceSnapshot::build(&root(), &t);
        assert_eq!(paths(&s), ["src", "src/main.rs", "Cargo.toml"]);
        assert!(!s.visible_tree[0].unexplored);
    }

    #[test]
    fn depth_is_rebased_so_the_roots_children_are_zero() {
        let mut t = tree();
        let src = t.visible_rows()[1].id;
        let _ = t.expand(src);
        t.set_children(
            src,
            vec![DirEntryInfo {
                name: "main.rs".into(),
                path: "/proj/src/main.rs".into(),
                kind: EntryKind::File,
            }],
        );

        let s = WorkspaceSnapshot::build(&root(), &t);
        assert_eq!(s.visible_tree[0].depth, 0, "src");
        assert_eq!(s.visible_tree[1].depth, 1, "src/main.rs");
    }

    #[test]
    fn the_selection_is_reported_relative_to_the_root() {
        let mut t = tree();
        let toml = t.visible_rows()[2].id;
        t.select(toml);

        let s = WorkspaceSnapshot::build(&root(), &t);
        assert_eq!(s.selection, Some(PathBuf::from("Cargo.toml")));
    }

    #[test]
    fn selecting_the_root_reports_no_selection() {
        let s = WorkspaceSnapshot::build(&root(), &tree());
        assert_eq!(s.selection, None, "the root is not a meaningful selection");
    }

    #[test]
    fn the_agent_sees_exactly_the_visible_rows_no_more() {
        // The premise of the whole design: one source of truth, so what is filtered out
        // of the human's view is filtered out of the agent's too.
        let t = tree();
        let s = WorkspaceSnapshot::build(&root(), &t);
        assert_eq!(
            s.len(),
            t.visible_rows().len() - 1,
            "every visible row but the root, and nothing else"
        );
    }

    #[test]
    fn a_collapsed_directory_hides_its_children_from_the_agent_too() {
        let mut t = tree();
        let src = t.visible_rows()[1].id;
        let _ = t.expand(src);
        t.set_children(
            src,
            vec![DirEntryInfo {
                name: "main.rs".into(),
                path: "/proj/src/main.rs".into(),
                kind: EntryKind::File,
            }],
        );
        t.collapse(src);

        let s = WorkspaceSnapshot::build(&root(), &t);
        assert_eq!(paths(&s), ["src", "Cargo.toml"]);
        assert!(s.visible_tree[0].unexplored, "collapsed reads as unexplored");
    }
}
