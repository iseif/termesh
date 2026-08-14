//! The lazy file-explorer tree (ADR-0005 §2).
//!
//! Pure data structure and pure logic: no I/O, no threads. The worker performs the
//! actual `read_dir`; this module decides *what* to read, absorbs the result, and
//! flattens the expanded portion into rows for the renderer.
//!
//! Nodes are an append-only arena keyed by [`NodeId`]. Identity is the id, never the
//! path (ARCHITECTURE.md §7.3) — which is precisely what lets a rename storm re-read a
//! directory without losing the user's selection or their expanded subtrees.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use termesh_core::NodeId;

use crate::service::{DirEntryInfo, EntryKind, FsError};

/// How much of a directory's contents we currently hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChildState {
    /// Never expanded. Costs nothing — this is what makes a monorepo root openable.
    Unloaded,
    /// A read is in flight on the worker thread.
    Loading,
    Loaded(Vec<NodeId>),
    /// The read failed; rendered inline on the node rather than aborting the tree.
    Error(FsError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    pub id: NodeId,
    pub parent: Option<NodeId>,
    pub name: OsString,
    pub path: PathBuf,
    pub kind: EntryKind,
    pub expanded: bool,
    pub children: ChildState,
    /// Tombstone: the entry vanished from disk. Kept so ids are never reused, and
    /// filtered out of every traversal.
    alive: bool,
}

impl Node {
    /// Only real directories can hold children. Symlinks are shown but not traversed
    /// through, so they never expand (ADR-0005 §6).
    pub fn is_expandable(&self) -> bool {
        self.kind == EntryKind::Dir
    }
}

/// One flattened, renderable line of the visible tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub id: NodeId,
    /// Nesting level; the root is 0.
    pub depth: usize,
    pub name: String,
    pub kind: EntryKind,
    pub expanded: bool,
    pub is_expandable: bool,
    /// Set when this node's expansion failed, for rendering the reason inline.
    pub error: Option<String>,
    pub loading: bool,
}

/// The explorer tree: structure, selection, and navigation. All pure logic.
#[derive(Debug, Clone)]
pub struct FileTree {
    nodes: Vec<Node>,
    root: NodeId,
    selected: NodeId,
}

impl FileTree {
    /// Build a tree containing just the root directory, collapsed and unloaded.
    pub fn new(root_path: impl Into<PathBuf>, display_name: impl Into<OsString>) -> Self {
        let root_path = root_path.into();
        let root = NodeId::new(0);
        let node = Node {
            id: root,
            parent: None,
            name: display_name.into(),
            path: root_path,
            kind: EntryKind::Dir,
            expanded: false,
            children: ChildState::Unloaded,
            alive: true,
        };
        Self { nodes: vec![node], root, selected: root }
    }

    pub fn root(&self) -> NodeId {
        self.root
    }

    pub fn selected(&self) -> NodeId {
        self.selected
    }

    pub fn node(&self, id: NodeId) -> Option<&Node> {
        self.nodes.get(id.0 as usize).filter(|n| n.alive)
    }

    fn node_mut(&mut self, id: NodeId) -> Option<&mut Node> {
        self.nodes.get_mut(id.0 as usize).filter(|n| n.alive)
    }

    pub fn path_of(&self, id: NodeId) -> Option<&Path> {
        self.node(id).map(|n| n.path.as_path())
    }

    /// Mark a directory as awaiting a read and hand back the path the worker should
    /// list. Returns `None` when nothing needs reading — not a directory, already
    /// loaded, or a read is already in flight — so callers never issue duplicate work.
    #[must_use]
    pub fn begin_load(&mut self, id: NodeId) -> Option<PathBuf> {
        let node = self.node_mut(id)?;
        if node.kind != EntryKind::Dir {
            return None;
        }
        match node.children {
            ChildState::Unloaded | ChildState::Error(_) => {
                node.children = ChildState::Loading;
                Some(node.path.clone())
            }
            ChildState::Loading | ChildState::Loaded(_) => None,
        }
    }

    /// Expand a directory. Returns a path if the contents still need to be read.
    #[must_use]
    pub fn expand(&mut self, id: NodeId) -> Option<PathBuf> {
        let node = self.node_mut(id)?;
        if node.kind != EntryKind::Dir {
            return None;
        }
        node.expanded = true;
        self.begin_load(id)
    }

    pub fn collapse(&mut self, id: NodeId) {
        if let Some(node) = self.node_mut(id) {
            node.expanded = false;
        }
    }

    /// Expand if collapsed, collapse if expanded. Returns a path needing a read.
    #[must_use]
    pub fn toggle(&mut self, id: NodeId) -> Option<PathBuf> {
        match self.node(id) {
            Some(n) if n.is_expandable() && n.expanded => {
                self.collapse(id);
                None
            }
            Some(n) if n.is_expandable() => self.expand(id),
            _ => None,
        }
    }

    /// Record a failed directory read against its node, leaving siblings untouched.
    pub fn set_error(&mut self, id: NodeId, error: FsError) {
        if let Some(node) = self.node_mut(id) {
            node.children = ChildState::Error(error);
        }
    }

    /// Absorb a directory listing, **reconciling** against whatever is already there.
    ///
    /// Entries matched by name keep their `NodeId`, their expanded flag, and their
    /// already-loaded descendants. Vanished entries are tombstoned. This is what makes
    /// a watch-triggered re-read non-destructive to the user's view (ADR-0005 §5) —
    /// re-reading a level is far simpler than patching the tree from event deltas, and
    /// this reconciliation is what makes that affordable.
    pub fn set_children(&mut self, id: NodeId, entries: Vec<DirEntryInfo>) {
        let Some(parent) = self.node(id) else { return };

        // Index the survivors by name so matching is O(n) rather than quadratic.
        let existing: HashMap<OsString, NodeId> = match &parent.children {
            ChildState::Loaded(ids) => ids
                .iter()
                .filter_map(|&cid| self.node(cid).map(|n| (n.name.clone(), cid)))
                .collect(),
            _ => HashMap::new(),
        };

        let mut new_children = Vec::with_capacity(entries.len());
        let mut kept = Vec::with_capacity(entries.len());

        for entry in entries {
            match existing.get(&entry.name) {
                // Same name and same kind: reuse the node wholesale.
                Some(&cid) if self.node(cid).map(|n| n.kind) == Some(entry.kind) => {
                    if let Some(n) = self.node_mut(cid) {
                        // The path can still shift if an ancestor was renamed.
                        n.path = entry.path;
                    }
                    kept.push(cid);
                    new_children.push(cid);
                }
                // Name reused for a different kind (file replaced by a directory):
                // that is a different thing, so it gets a fresh identity.
                _ => new_children.push(self.push_node(Some(id), entry)),
            }
        }

        // Anything previously present and not kept has gone from disk.
        for (_, cid) in existing {
            if !kept.contains(&cid) {
                self.kill_subtree(cid);
            }
        }

        if let Some(node) = self.node_mut(id) {
            node.children = ChildState::Loaded(new_children);
        }

        // The selection may have just been tombstoned; fall back to something real.
        if self.node(self.selected).is_none() {
            self.selected = self.nearest_live_ancestor(id);
        }
    }

    fn push_node(&mut self, parent: Option<NodeId>, entry: DirEntryInfo) -> NodeId {
        let id = NodeId::new(self.nodes.len() as u64);
        self.nodes.push(Node {
            id,
            parent,
            name: entry.name,
            path: entry.path,
            kind: entry.kind,
            expanded: false,
            children: ChildState::Unloaded,
            alive: true,
        });
        id
    }

    /// Tombstone a node and everything beneath it. Ids are never reused.
    fn kill_subtree(&mut self, id: NodeId) {
        let mut stack = vec![id];
        while let Some(cur) = stack.pop() {
            let Some(node) = self.nodes.get_mut(cur.0 as usize) else { continue };
            node.alive = false;
            if let ChildState::Loaded(kids) = &node.children {
                stack.extend(kids.iter().copied());
            }
        }
    }

    fn nearest_live_ancestor(&self, from: NodeId) -> NodeId {
        let mut cur = Some(from);
        while let Some(id) = cur {
            if self.node(id).is_some() {
                return id;
            }
            cur = self.nodes.get(id.0 as usize).and_then(|n| n.parent);
        }
        self.root
    }

    /// The expanded tree flattened into render order (depth-first, parents before children).
    pub fn visible_rows(&self) -> Vec<Row> {
        let mut rows = Vec::new();
        self.push_rows(self.root, 0, &mut rows);
        rows
    }

    fn push_rows(&self, id: NodeId, depth: usize, out: &mut Vec<Row>) {
        let Some(node) = self.node(id) else { return };
        out.push(Row {
            id,
            depth,
            name: node.name.to_string_lossy().into_owned(),
            kind: node.kind,
            expanded: node.expanded,
            is_expandable: node.is_expandable(),
            error: match &node.children {
                ChildState::Error(e) => Some(e.to_string()),
                _ => None,
            },
            loading: matches!(node.children, ChildState::Loading),
        });

        if !node.expanded {
            return;
        }
        if let ChildState::Loaded(children) = &node.children {
            for &child in children {
                self.push_rows(child, depth + 1, out);
            }
        }
    }

    /// Find a live node by its path. Linear, but only ever over *loaded* nodes, which
    /// is bounded by what the user has actually expanded.
    pub fn find_by_path(&self, path: &Path) -> Option<NodeId> {
        self.nodes.iter().find(|n| n.alive && n.path == path).map(|n| n.id)
    }

    /// Given paths that changed on disk, decide which loaded directories to re-read.
    ///
    /// A change to `/r/src/main.rs` means re-reading `/r/src`; a change to `/r/src`
    /// itself means the same. Directories we have never loaded are skipped — there is
    /// nothing to refresh, and expanding them later will read them anyway. The result is
    /// deduplicated so a burst of edits in one directory costs one read (ADR-0005 §5).
    pub fn dirs_to_refresh(&self, changed: &[PathBuf]) -> Vec<NodeId> {
        let mut out: Vec<NodeId> = Vec::new();
        for path in changed {
            // The changed path itself if it is a loaded directory, otherwise its parent.
            let candidate = self
                .find_by_path(path)
                .filter(|&id| self.is_loaded_dir(id))
                .or_else(|| path.parent().and_then(|p| self.find_by_path(p)))
                .filter(|&id| self.is_loaded_dir(id));

            if let Some(id) = candidate {
                if !out.contains(&id) {
                    out.push(id);
                }
            }
        }
        out
    }

    fn is_loaded_dir(&self, id: NodeId) -> bool {
        self.node(id).is_some_and(|n| matches!(n.children, ChildState::Loaded(_)))
    }

    /// Every directory whose contents are currently materialized, expanded or not.
    /// Configuration reload uses this to reapply exclusion rules immediately without
    /// walking any directory the user never opened.
    pub fn loaded_directories(&self) -> Vec<(NodeId, PathBuf)> {
        self.nodes
            .iter()
            .filter(|node| {
                node.alive
                    && node.kind == EntryKind::Dir
                    && matches!(node.children, ChildState::Loaded(_))
            })
            .map(|node| (node.id, node.path.clone()))
            .collect()
    }

    /// Queue a re-read of an already-loaded directory, keeping the current contents
    /// visible until the new listing arrives. Returns the path to read.
    #[must_use]
    pub fn refresh(&mut self, id: NodeId) -> Option<PathBuf> {
        self.node(id).filter(|n| n.kind == EntryKind::Dir).map(|n| n.path.clone())
    }

    // --- selection / navigation -------------------------------------------------

    pub fn select(&mut self, id: NodeId) {
        if self.node(id).is_some() {
            self.selected = id;
        }
    }

    /// Index of the selection within [`Self::visible_rows`], for scrolling and highlight.
    pub fn selected_row(&self) -> usize {
        self.visible_rows().iter().position(|r| r.id == self.selected).unwrap_or(0)
    }

    pub fn select_next(&mut self) {
        self.move_selection(1);
    }

    pub fn select_prev(&mut self) {
        self.move_selection(-1);
    }

    fn move_selection(&mut self, delta: isize) {
        let rows = self.visible_rows();
        if rows.is_empty() {
            return;
        }
        let cur = rows.iter().position(|r| r.id == self.selected).unwrap_or(0) as isize;
        // Clamp rather than wrap: arrowing past the end of a file list should rest at
        // the end, not jump back to the top.
        let next = (cur + delta).clamp(0, rows.len() as isize - 1) as usize;
        self.selected = rows[next].id;
    }

    /// Collapse the selection, or step to its parent when it is already collapsed —
    /// the conventional left-arrow behaviour in a tree.
    pub fn collapse_or_parent(&mut self) {
        let Some(node) = self.node(self.selected) else { return };
        if node.is_expandable() && node.expanded {
            let id = node.id;
            self.collapse(id);
        } else if let Some(parent) = node.parent {
            self.selected = parent;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, kind: EntryKind) -> DirEntryInfo {
        DirEntryInfo { name: name.into(), path: PathBuf::from("/r").join(name), kind }
    }

    fn dir(name: &str) -> DirEntryInfo {
        entry(name, EntryKind::Dir)
    }
    fn file(name: &str) -> DirEntryInfo {
        entry(name, EntryKind::File)
    }

    fn tree() -> FileTree {
        FileTree::new("/r", "r")
    }

    fn names(t: &FileTree) -> Vec<String> {
        t.visible_rows().iter().map(|r| r.name.clone()).collect()
    }

    #[test]
    fn a_new_tree_shows_only_its_root() {
        let t = tree();
        assert_eq!(names(&t), ["r"]);
        assert_eq!(t.selected(), t.root());
    }

    #[test]
    fn expanding_requests_a_read_then_shows_children() {
        let mut t = tree();
        assert_eq!(t.expand(t.root()), Some(PathBuf::from("/r")), "needs a read");
        assert!(t.visible_rows()[0].loading, "renders as loading meanwhile");

        t.set_children(t.root(), vec![dir("src"), file("Cargo.toml")]);
        assert_eq!(names(&t), ["r", "src", "Cargo.toml"]);
    }

    #[test]
    fn expanding_an_already_loaded_directory_does_not_re_read() {
        let mut t = tree();
        let _ = t.expand(t.root());
        t.set_children(t.root(), vec![dir("src")]);
        t.collapse(t.root());
        assert_eq!(t.expand(t.root()), None, "contents are still held; no duplicate work");
    }

    #[test]
    fn collapsed_subtrees_contribute_no_rows() {
        let mut t = tree();
        let _ = t.expand(t.root());
        t.set_children(t.root(), vec![dir("src")]);
        let src = t.visible_rows()[1].id;
        let _ = t.expand(src);
        t.set_children(src, vec![file("main.rs")]);
        assert_eq!(names(&t), ["r", "src", "main.rs"]);

        t.collapse(src);
        assert_eq!(names(&t), ["r", "src"], "children of a collapsed dir are not rendered");
    }

    #[test]
    fn files_never_expand() {
        let mut t = tree();
        let _ = t.expand(t.root());
        t.set_children(t.root(), vec![file("README.md")]);
        let readme = t.visible_rows()[1].id;
        assert_eq!(t.expand(readme), None);
        assert!(!t.visible_rows()[1].is_expandable);
    }

    #[test]
    fn symlinks_are_not_expandable() {
        let mut t = tree();
        let _ = t.expand(t.root());
        t.set_children(t.root(), vec![entry("link", EntryKind::Symlink)]);
        let link = t.visible_rows()[1].id;
        assert_eq!(t.expand(link), None, "symlinks are shown but not traversed");
    }

    #[test]
    fn a_failed_read_is_recorded_on_the_node_not_fatal() {
        let mut t = tree();
        let _ = t.expand(t.root());
        t.set_children(t.root(), vec![dir("secret"), file("ok.txt")]);
        let secret = t.visible_rows()[1].id;
        let _ = t.expand(secret);
        t.set_error(secret, FsError::PermissionDenied(PathBuf::from("/r/secret")));

        let rows = t.visible_rows();
        assert!(rows[1].error.as_ref().unwrap().contains("permission denied"));
        assert_eq!(rows[2].name, "ok.txt", "the sibling still renders");
    }

    #[test]
    fn a_failed_read_can_be_retried() {
        let mut t = tree();
        let _ = t.expand(t.root());
        t.set_error(t.root(), FsError::PermissionDenied(PathBuf::from("/r")));
        assert_eq!(t.begin_load(t.root()), Some(PathBuf::from("/r")), "errors are retryable");
    }

    // --- reconciliation: the point of the arena ---------------------------------

    #[test]
    fn re_reading_preserves_node_identity_and_expansion() {
        let mut t = tree();
        let _ = t.expand(t.root());
        t.set_children(t.root(), vec![dir("src"), file("a.txt")]);
        let src = t.visible_rows()[1].id;
        let _ = t.expand(src);
        t.set_children(src, vec![file("main.rs")]);

        // A watch event re-reads the root with the same entries plus one.
        t.set_children(t.root(), vec![dir("src"), file("a.txt"), file("b.txt")]);

        assert_eq!(t.visible_rows()[1].id, src, "src keeps its NodeId");
        assert_eq!(
            names(&t),
            ["r", "src", "main.rs", "a.txt", "b.txt"],
            "src stays expanded and keeps its loaded children"
        );
    }

    #[test]
    fn vanished_entries_disappear_from_the_tree() {
        let mut t = tree();
        let _ = t.expand(t.root());
        t.set_children(t.root(), vec![file("gone.txt"), file("stays.txt")]);
        t.set_children(t.root(), vec![file("stays.txt")]);
        assert_eq!(names(&t), ["r", "stays.txt"]);
    }

    #[test]
    fn ids_are_never_reused_after_a_delete() {
        let mut t = tree();
        let _ = t.expand(t.root());
        t.set_children(t.root(), vec![file("gone.txt")]);
        let gone = t.visible_rows()[1].id;

        t.set_children(t.root(), vec![file("new.txt")]);
        let new = t.visible_rows()[1].id;
        assert_ne!(gone, new, "a fresh entry must not inherit a dead id");
        assert!(t.node(gone).is_none(), "the dead node is unreachable");
    }

    #[test]
    fn a_name_reused_for_a_different_kind_gets_a_fresh_identity() {
        let mut t = tree();
        let _ = t.expand(t.root());
        t.set_children(t.root(), vec![file("thing")]);
        let as_file = t.visible_rows()[1].id;

        t.set_children(t.root(), vec![dir("thing")]);
        let as_dir = t.visible_rows()[1].id;
        assert_ne!(as_file, as_dir, "file replaced by a directory is a different thing");
        assert!(t.visible_rows()[1].is_expandable);
    }

    #[test]
    fn selection_survives_a_re_read_that_keeps_it() {
        let mut t = tree();
        let _ = t.expand(t.root());
        t.set_children(t.root(), vec![file("a.txt"), file("b.txt")]);
        let b = t.visible_rows()[2].id;
        t.select(b);

        t.set_children(t.root(), vec![file("a.txt"), file("b.txt"), file("c.txt")]);
        assert_eq!(t.selected(), b, "selection is untouched by unrelated churn");
    }

    #[test]
    fn selection_falls_back_when_the_selected_node_is_deleted() {
        let mut t = tree();
        let _ = t.expand(t.root());
        t.set_children(t.root(), vec![file("doomed.txt")]);
        let doomed = t.visible_rows()[1].id;
        t.select(doomed);

        t.set_children(t.root(), vec![file("other.txt")]);
        assert_eq!(t.selected(), t.root(), "falls back to a live ancestor, never dangles");
        assert!(t.node(t.selected()).is_some());
    }

    // --- navigation --------------------------------------------------------------

    #[test]
    fn navigation_walks_visible_rows_and_clamps_at_the_ends() {
        let mut t = tree();
        let _ = t.expand(t.root());
        t.set_children(t.root(), vec![file("a"), file("b")]);

        t.select_next();
        assert_eq!(t.selected_row(), 1);
        t.select_next();
        assert_eq!(t.selected_row(), 2);
        t.select_next();
        assert_eq!(t.selected_row(), 2, "clamps at the bottom rather than wrapping");

        t.select_prev();
        t.select_prev();
        t.select_prev();
        assert_eq!(t.selected_row(), 0, "clamps at the top");
    }

    #[test]
    fn navigation_skips_collapsed_children() {
        let mut t = tree();
        let _ = t.expand(t.root());
        t.set_children(t.root(), vec![dir("src"), file("z.txt")]);
        let src = t.visible_rows()[1].id;
        let _ = t.expand(src);
        t.set_children(src, vec![file("hidden_when_collapsed.rs")]);
        t.collapse(src);

        t.select(src);
        t.select_next();
        assert_eq!(t.visible_rows()[t.selected_row()].name, "z.txt");
    }

    #[test]
    fn left_collapses_then_steps_to_the_parent() {
        let mut t = tree();
        let _ = t.expand(t.root());
        t.set_children(t.root(), vec![dir("src")]);
        let src = t.visible_rows()[1].id;
        let _ = t.expand(src);
        t.set_children(src, vec![file("main.rs")]);
        t.select(src);

        t.collapse_or_parent();
        assert!(!t.visible_rows()[1].expanded, "first press collapses");
        t.collapse_or_parent();
        assert_eq!(t.selected(), t.root(), "second press moves to the parent");
    }

    // --- watch-event routing -----------------------------------------------------

    fn loaded_tree() -> (FileTree, NodeId) {
        let mut t = tree();
        let _ = t.expand(t.root());
        t.set_children(t.root(), vec![dir("src"), file("a.txt")]);
        let src = t.visible_rows()[1].id;
        let _ = t.expand(src);
        t.set_children(src, vec![file("main.rs")]);
        (t, src)
    }

    #[test]
    fn a_changed_file_refreshes_its_containing_directory() {
        let (t, src) = loaded_tree();
        let changed = vec![PathBuf::from("/r/src/main.rs")];
        assert_eq!(t.dirs_to_refresh(&changed), vec![src]);
    }

    #[test]
    fn a_changed_directory_refreshes_itself() {
        let (t, src) = loaded_tree();
        assert_eq!(t.dirs_to_refresh(&[PathBuf::from("/r/src")]), vec![src]);
    }

    #[test]
    fn a_burst_in_one_directory_coalesces_to_one_read() {
        let (t, src) = loaded_tree();
        let changed = vec![
            PathBuf::from("/r/src/main.rs"),
            PathBuf::from("/r/src/other.rs"),
            PathBuf::from("/r/src/third.rs"),
        ];
        assert_eq!(t.dirs_to_refresh(&changed), vec![src], "deduplicated to one read");
    }

    #[test]
    fn changes_under_unloaded_directories_are_ignored() {
        let mut t = tree();
        let _ = t.expand(t.root());
        t.set_children(t.root(), vec![dir("never_opened")]);
        // The directory exists in the tree but was never expanded, so there is nothing
        // to reconcile and no read worth doing.
        let changed = vec![PathBuf::from("/r/never_opened/whatever.rs")];
        assert!(t.dirs_to_refresh(&changed).is_empty());
    }

    #[test]
    fn changes_outside_the_tree_are_ignored() {
        let (t, _) = loaded_tree();
        assert!(t.dirs_to_refresh(&[PathBuf::from("/somewhere/else/x.rs")]).is_empty());
    }

    #[test]
    fn find_by_path_only_returns_live_nodes() {
        let mut t = tree();
        let _ = t.expand(t.root());
        t.set_children(t.root(), vec![file("doomed.txt")]);
        assert!(t.find_by_path(Path::new("/r/doomed.txt")).is_some());

        t.set_children(t.root(), vec![]);
        assert!(t.find_by_path(Path::new("/r/doomed.txt")).is_none(), "tombstones stay hidden");
    }

    #[test]
    fn toggle_expands_then_collapses() {
        let mut t = tree();
        assert!(t.toggle(t.root()).is_some(), "first toggle asks for a read");
        t.set_children(t.root(), vec![file("a")]);
        assert_eq!(names(&t), ["r", "a"]);
        assert!(t.toggle(t.root()).is_none());
        assert_eq!(names(&t), ["r"]);
    }
}
