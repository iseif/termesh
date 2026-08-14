//! The `std::fs`-backed [`FileSystemService`]. The one place in the workspace that is
//! allowed to touch `std::fs` — everything else goes through the trait (ARCHITECTURE.md §7.4).

use std::io;
use std::path::{Path, PathBuf};

use crate::service::{sort_entries, DirEntryInfo, EntryKind, FileSystemService, FsError, FsResult};

/// Reads and writes the actual filesystem. Stateless, so it is cheap to share across
/// the worker thread and any future services that need it.
#[derive(Debug, Clone, Copy, Default)]
pub struct RealFileSystem;

impl RealFileSystem {
    pub const fn new() -> Self {
        Self
    }
}

/// Translate an `io::Error` into our vocabulary, attaching the path it concerns.
fn map_err(path: &Path, e: io::Error) -> FsError {
    match e.kind() {
        io::ErrorKind::NotFound => FsError::NotFound(path.to_path_buf()),
        io::ErrorKind::PermissionDenied => FsError::PermissionDenied(path.to_path_buf()),
        io::ErrorKind::AlreadyExists => FsError::AlreadyExists(path.to_path_buf()),
        _ => FsError::Other { path: path.to_path_buf(), message: e.to_string() },
    }
}

impl FileSystemService for RealFileSystem {
    fn read_dir(&self, path: &Path) -> FsResult<Vec<DirEntryInfo>> {
        let mut out = Vec::new();
        for entry in std::fs::read_dir(path).map_err(|e| map_err(path, e))? {
            // A single unreadable entry must not sink the whole listing — skip it and
            // keep its siblings (ADR-0005 §6).
            let Ok(entry) = entry else { continue };
            let entry_path = entry.path();

            // `file_type()` on the DirEntry does not follow symlinks, which is what we
            // want: a symlinked directory is shown but not traversed through.
            let kind = match entry.file_type() {
                Ok(ft) if ft.is_symlink() => EntryKind::Symlink,
                Ok(ft) if ft.is_dir() => EntryKind::Dir,
                Ok(_) => EntryKind::File,
                Err(_) => continue,
            };

            out.push(DirEntryInfo { name: entry.file_name(), path: entry_path, kind });
        }
        sort_entries(&mut out);
        Ok(out)
    }

    fn read_file(&self, path: &Path) -> FsResult<Vec<u8>> {
        std::fs::read(path).map_err(|e| map_err(path, e))
    }

    fn create_file(&self, path: &Path) -> FsResult<()> {
        // create_new: never truncate an existing file out from under someone.
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map(|_| ())
            .map_err(|e| map_err(path, e))
    }

    fn write_file(&self, path: &Path, contents: &[u8]) -> FsResult<()> {
        std::fs::write(path, contents).map_err(|e| map_err(path, e))
    }

    fn create_dir(&self, path: &Path) -> FsResult<()> {
        std::fs::create_dir_all(path).map_err(|e| map_err(path, e))
    }

    fn rename(&self, from: &Path, to: &Path) -> FsResult<()> {
        std::fs::rename(from, to).map_err(|e| map_err(from, e))
    }

    fn remove_file(&self, path: &Path) -> FsResult<()> {
        std::fs::remove_file(path).map_err(|e| map_err(path, e))
    }

    fn remove_dir_all(&self, path: &Path) -> FsResult<()> {
        std::fs::remove_dir_all(path).map_err(|e| map_err(path, e))
    }

    fn canonicalize(&self, path: &Path) -> FsResult<PathBuf> {
        std::fs::canonicalize(path).map_err(|e| map_err(path, e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch directory under the OS temp dir, removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            // Nanosecond stamp + tag keeps parallel test threads from colliding without
            // pulling in a tempfile dependency for four tests.
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let p = std::env::temp_dir().join(format!("termesh-fs-{tag}-{stamp}"));
            std::fs::create_dir_all(&p).unwrap();
            Self(p)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn read_dir_lists_one_level_sorted_dirs_first() {
        let tmp = TempDir::new("readdir");
        let fs = RealFileSystem::new();
        fs.create_dir(&tmp.path().join("src/nested")).unwrap();
        fs.create_file(&tmp.path().join("Cargo.toml")).unwrap();
        fs.create_file(&tmp.path().join("README.md")).unwrap();

        let entries = fs.read_dir(tmp.path()).unwrap();
        let names: Vec<_> = entries.iter().map(|e| e.name.to_string_lossy().into_owned()).collect();
        assert_eq!(names, ["src", "Cargo.toml", "README.md"], "one level only, dirs first");
        assert_eq!(entries[0].kind, EntryKind::Dir);
    }

    #[test]
    fn create_file_refuses_to_clobber() {
        let tmp = TempDir::new("clobber");
        let fs = RealFileSystem::new();
        let f = tmp.path().join("a.txt");
        fs.create_file(&f).unwrap();
        std::fs::write(&f, b"precious").unwrap();

        assert_eq!(fs.create_file(&f), Err(FsError::AlreadyExists(f.clone())));
        assert_eq!(fs.read_file(&f).unwrap(), b"precious", "existing content is untouched");
    }

    #[test]
    fn missing_paths_report_not_found() {
        let tmp = TempDir::new("missing");
        let fs = RealFileSystem::new();
        let missing = tmp.path().join("nope");
        assert_eq!(fs.read_dir(&missing), Err(FsError::NotFound(missing.clone())));
        assert_eq!(fs.read_file(&missing), Err(FsError::NotFound(missing)));
    }

    #[test]
    fn rename_and_remove_round_trip() {
        let tmp = TempDir::new("rename");
        let fs = RealFileSystem::new();
        let (a, b) = (tmp.path().join("a.txt"), tmp.path().join("b.txt"));
        fs.create_file(&a).unwrap();
        fs.rename(&a, &b).unwrap();

        let names: Vec<_> =
            fs.read_dir(tmp.path()).unwrap().iter().map(|e| e.name.clone()).collect();
        assert_eq!(names, ["b.txt"]);

        fs.remove_file(&b).unwrap();
        assert!(fs.read_dir(tmp.path()).unwrap().is_empty());
    }

    #[test]
    fn symlinks_are_reported_as_symlinks_not_followed() {
        // Unix-only: creating symlinks on Windows needs elevated privileges.
        #[cfg(unix)]
        {
            let tmp = TempDir::new("symlink");
            let fs = RealFileSystem::new();
            fs.create_dir(&tmp.path().join("target")).unwrap();
            std::os::unix::fs::symlink(tmp.path().join("target"), tmp.path().join("link")).unwrap();

            let entries = fs.read_dir(tmp.path()).unwrap();
            let link = entries.iter().find(|e| e.name == "link").unwrap();
            assert_eq!(link.kind, EntryKind::Symlink, "must not resolve to Dir");
        }
    }
}
