//! An in-memory [`FileSystemService`] so explorer logic is testable without a disk.
//!
//! Required by CONTRIBUTING.md ("every new service ships with a fake") and load-bearing for
//! CI: the `--dump-frame` snapshot must not depend on whatever happens to be on the
//! filesystem of the machine rendering it (ADR-0005, Consequences).

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;

use termesh_filesystem::{
    sort_entries, DirEntryInfo, EntryKind, FileSystemService, FsError, FsResult,
};

/// What lives at a path in the fake tree.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Node {
    Dir,
    File(Vec<u8>),
    /// Stores its target, but is never traversed — same contract as the real service.
    Symlink(PathBuf),
}

/// An in-memory filesystem.
///
/// Paths are normalised (`.` and `..` resolved lexically) so callers can hand it either
/// absolute or relative paths and get consistent answers. Interior mutability via
/// `Mutex` keeps it usable behind the `&self` trait methods and `Send + Sync` for the
/// worker thread.
#[derive(Debug, Default)]
pub struct FakeFileSystem {
    inner: Mutex<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    nodes: BTreeMap<PathBuf, Node>,
    /// Errors injected for specific paths, to exercise failure rendering.
    failures: BTreeMap<PathBuf, FsError>,
}

impl FakeFileSystem {
    pub fn new() -> Self {
        let mut inner = Inner::default();
        inner.nodes.insert(PathBuf::from("/"), Node::Dir);
        Self { inner: Mutex::new(inner) }
    }

    /// Build a tree from a path list. Trailing `/` means directory; parents are implied.
    ///
    /// ```ignore
    /// FakeFileSystem::with_paths(&["/proj/src/main.rs", "/proj/Cargo.toml", "/proj/target/"]);
    /// ```
    pub fn with_paths(paths: &[&str]) -> Self {
        let fs = Self::new();
        for p in paths {
            if let Some(dir) = p.strip_suffix('/') {
                fs.add_dir(dir);
            } else {
                fs.add_file(p, b"");
            }
        }
        fs
    }

    /// Insert a directory and every missing ancestor.
    pub fn add_dir(&self, path: impl AsRef<Path>) -> &Self {
        let path = normalize(path.as_ref());
        let mut inner = self.inner.lock().unwrap();
        insert_ancestors(&mut inner.nodes, &path);
        inner.nodes.insert(path, Node::Dir);
        drop(inner);
        self
    }

    /// Insert a file with contents, creating missing ancestor directories.
    pub fn add_file(&self, path: impl AsRef<Path>, contents: &[u8]) -> &Self {
        let path = normalize(path.as_ref());
        let mut inner = self.inner.lock().unwrap();
        insert_ancestors(&mut inner.nodes, &path);
        inner.nodes.insert(path, Node::File(contents.to_vec()));
        drop(inner);
        self
    }

    /// Insert a symlink. It is listed but never traversed, matching the real service.
    pub fn add_symlink(&self, path: impl AsRef<Path>, target: impl AsRef<Path>) -> &Self {
        let path = normalize(path.as_ref());
        let mut inner = self.inner.lock().unwrap();
        insert_ancestors(&mut inner.nodes, &path);
        inner.nodes.insert(path, Node::Symlink(target.as_ref().to_path_buf()));
        drop(inner);
        self
    }

    /// Make every operation on `path` fail with `error` — for exercising the explorer's
    /// permission-denied and I/O-error rendering without needing a real unreadable dir.
    pub fn fail(&self, path: impl AsRef<Path>, error: FsError) -> &Self {
        let path = normalize(path.as_ref());
        self.inner.lock().unwrap().failures.insert(path, error);
        self
    }

    /// Every path currently in the tree, sorted. Useful for asserting after mutations.
    pub fn paths(&self) -> Vec<PathBuf> {
        self.inner.lock().unwrap().nodes.keys().cloned().collect()
    }
}

impl Inner {
    fn check_failure(&self, path: &Path) -> FsResult<()> {
        match self.failures.get(path) {
            Some(e) => Err(e.clone()),
            None => Ok(()),
        }
    }
}

/// Resolve `.` and `..` lexically. Purely textual — we have no symlinks to chase.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    if out.as_os_str().is_empty() {
        PathBuf::from("/")
    } else {
        out
    }
}

fn insert_ancestors(nodes: &mut BTreeMap<PathBuf, Node>, path: &Path) {
    let mut cur = PathBuf::new();
    for c in path.components() {
        cur.push(c.as_os_str());
        if cur != path {
            nodes.entry(cur.clone()).or_insert(Node::Dir);
        }
    }
}

fn kind_of(node: &Node) -> EntryKind {
    match node {
        Node::Dir => EntryKind::Dir,
        Node::File(_) => EntryKind::File,
        Node::Symlink(_) => EntryKind::Symlink,
    }
}

impl FileSystemService for FakeFileSystem {
    fn read_dir(&self, path: &Path) -> FsResult<Vec<DirEntryInfo>> {
        let path = normalize(path);
        let inner = self.inner.lock().unwrap();
        inner.check_failure(&path)?;

        match inner.nodes.get(&path) {
            None => return Err(FsError::NotFound(path)),
            Some(Node::Dir) => {}
            Some(_) => return Err(FsError::NotADirectory(path)),
        }

        // Direct children only — the tree is lazy, so we never recurse here.
        let mut out: Vec<DirEntryInfo> = inner
            .nodes
            .iter()
            .filter(|(p, _)| p.parent() == Some(path.as_path()))
            .map(|(p, node)| DirEntryInfo {
                name: p.file_name().unwrap_or_default().to_os_string(),
                path: p.clone(),
                kind: kind_of(node),
            })
            .collect();
        sort_entries(&mut out);
        Ok(out)
    }

    fn read_file(&self, path: &Path) -> FsResult<Vec<u8>> {
        let path = normalize(path);
        let inner = self.inner.lock().unwrap();
        inner.check_failure(&path)?;
        match inner.nodes.get(&path) {
            Some(Node::File(bytes)) => Ok(bytes.clone()),
            Some(_) => Err(FsError::Other { path, message: "not a regular file".to_string() }),
            None => Err(FsError::NotFound(path)),
        }
    }

    fn create_file(&self, path: &Path) -> FsResult<()> {
        let path = normalize(path);
        let mut inner = self.inner.lock().unwrap();
        inner.check_failure(&path)?;
        if inner.nodes.contains_key(&path) {
            return Err(FsError::AlreadyExists(path));
        }
        insert_ancestors(&mut inner.nodes, &path);
        inner.nodes.insert(path, Node::File(Vec::new()));
        Ok(())
    }

    fn write_file(&self, path: &Path, contents: &[u8]) -> FsResult<()> {
        let path = normalize(path);
        let mut inner = self.inner.lock().unwrap();
        inner.check_failure(&path)?;
        insert_ancestors(&mut inner.nodes, &path);
        inner.nodes.insert(path, Node::File(contents.to_vec()));
        Ok(())
    }

    fn create_dir(&self, path: &Path) -> FsResult<()> {
        let path = normalize(path);
        let mut inner = self.inner.lock().unwrap();
        inner.check_failure(&path)?;
        insert_ancestors(&mut inner.nodes, &path);
        inner.nodes.entry(path).or_insert(Node::Dir);
        Ok(())
    }

    fn rename(&self, from: &Path, to: &Path) -> FsResult<()> {
        let (from, to) = (normalize(from), normalize(to));
        let mut inner = self.inner.lock().unwrap();
        inner.check_failure(&from)?;
        if !inner.nodes.contains_key(&from) {
            return Err(FsError::NotFound(from));
        }
        if inner.nodes.contains_key(&to) {
            return Err(FsError::AlreadyExists(to));
        }
        // Move the subtree: the node itself plus everything beneath it.
        let moving: Vec<PathBuf> =
            inner.nodes.keys().filter(|p| p.starts_with(&from)).cloned().collect();
        for old in moving {
            let node = inner.nodes.remove(&old).expect("key came from this map");
            let suffix = old.strip_prefix(&from).expect("filtered by starts_with");
            inner.nodes.insert(to.join(suffix), node);
        }
        insert_ancestors(&mut inner.nodes, &to);
        Ok(())
    }

    fn remove_file(&self, path: &Path) -> FsResult<()> {
        let path = normalize(path);
        let mut inner = self.inner.lock().unwrap();
        inner.check_failure(&path)?;
        match inner.nodes.get(&path) {
            Some(Node::Dir) => Err(FsError::Other { path, message: "is a directory".to_string() }),
            Some(_) => {
                inner.nodes.remove(&path);
                Ok(())
            }
            None => Err(FsError::NotFound(path)),
        }
    }

    fn remove_dir_all(&self, path: &Path) -> FsResult<()> {
        let path = normalize(path);
        let mut inner = self.inner.lock().unwrap();
        inner.check_failure(&path)?;
        if !inner.nodes.contains_key(&path) {
            return Err(FsError::NotFound(path));
        }
        inner.nodes.retain(|p, _| !p.starts_with(&path));
        Ok(())
    }

    fn canonicalize(&self, path: &Path) -> FsResult<PathBuf> {
        let path = normalize(path);
        let inner = self.inner.lock().unwrap();
        inner.check_failure(&path)?;
        if inner.nodes.contains_key(&path) {
            Ok(path)
        } else {
            Err(FsError::NotFound(path))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> FakeFileSystem {
        FakeFileSystem::with_paths(&[
            "/proj/src/main.rs",
            "/proj/src/model.rs",
            "/proj/Cargo.toml",
            "/proj/target/debug/",
        ])
    }

    fn names(fs: &FakeFileSystem, at: &str) -> Vec<String> {
        fs.read_dir(Path::new(at))
            .unwrap()
            .iter()
            .map(|e| e.name.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn read_dir_returns_direct_children_only_dirs_first() {
        assert_eq!(names(&sample(), "/proj"), ["src", "target", "Cargo.toml"]);
    }

    #[test]
    fn read_dir_does_not_recurse() {
        // main.rs lives one level down and must not appear in /proj's listing.
        assert!(!names(&sample(), "/proj").contains(&"main.rs".to_string()));
        assert_eq!(names(&sample(), "/proj/src"), ["main.rs", "model.rs"]);
    }

    #[test]
    fn ordering_matches_the_real_service_contract() {
        let fs = FakeFileSystem::with_paths(&["/r/README.md", "/r/assets/", "/r/Cargo.toml"]);
        assert_eq!(names(&fs, "/r"), ["assets", "Cargo.toml", "README.md"]);
    }

    #[test]
    fn injected_failures_surface_on_read() {
        let fs = sample();
        let denied = PathBuf::from("/proj/src");
        fs.fail(&denied, FsError::PermissionDenied(denied.clone()));
        assert_eq!(fs.read_dir(&denied), Err(FsError::PermissionDenied(denied)));
        // Siblings are unaffected.
        assert!(fs.read_dir(Path::new("/proj")).is_ok());
    }

    #[test]
    fn missing_and_wrong_kind_are_distinguishable() {
        let fs = sample();
        assert_eq!(
            fs.read_dir(Path::new("/proj/nope")),
            Err(FsError::NotFound(PathBuf::from("/proj/nope")))
        );
        assert_eq!(
            fs.read_dir(Path::new("/proj/Cargo.toml")),
            Err(FsError::NotADirectory(PathBuf::from("/proj/Cargo.toml")))
        );
    }

    #[test]
    fn create_file_refuses_to_clobber() {
        let fs = sample();
        fs.add_file("/proj/keep.txt", b"precious");
        assert_eq!(
            fs.create_file(Path::new("/proj/keep.txt")),
            Err(FsError::AlreadyExists(PathBuf::from("/proj/keep.txt")))
        );
        assert_eq!(fs.read_file(Path::new("/proj/keep.txt")).unwrap(), b"precious");
    }

    #[test]
    fn rename_moves_the_whole_subtree() {
        let fs = sample();
        fs.rename(Path::new("/proj/src"), Path::new("/proj/lib")).unwrap();
        assert_eq!(names(&fs, "/proj/lib"), ["main.rs", "model.rs"]);
        assert_eq!(
            fs.read_dir(Path::new("/proj/src")),
            Err(FsError::NotFound(PathBuf::from("/proj/src")))
        );
    }

    #[test]
    fn remove_dir_all_takes_descendants_but_not_siblings() {
        let fs = sample();
        fs.remove_dir_all(Path::new("/proj/src")).unwrap();
        assert!(!fs.paths().iter().any(|p| p.starts_with("/proj/src")));
        assert!(fs.paths().contains(&PathBuf::from("/proj/Cargo.toml")));
    }

    #[test]
    fn remove_file_refuses_directories() {
        let fs = sample();
        assert!(fs.remove_file(Path::new("/proj/src")).is_err());
        assert!(fs.read_dir(Path::new("/proj/src")).is_ok(), "directory survives");
    }

    #[test]
    fn symlinks_are_listed_but_typed_as_symlinks() {
        let fs = sample();
        fs.add_symlink("/proj/link", "/proj/src");
        let entries = fs.read_dir(Path::new("/proj")).unwrap();
        let link = entries.iter().find(|e| e.name == "link").unwrap();
        assert_eq!(link.kind, EntryKind::Symlink);
    }

    #[test]
    fn paths_are_normalized_before_lookup() {
        let fs = sample();
        assert_eq!(names(&fs, "/proj/src/../src"), ["main.rs", "model.rs"]);
        assert_eq!(names(&fs, "/proj/./src"), ["main.rs", "model.rs"]);
    }
}
