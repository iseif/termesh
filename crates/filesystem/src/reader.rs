//! Reading one directory level *the way the explorer wants it*: listed, ignore-filtered,
//! and capped.
//!
//! Exists so the worker thread and the synchronous headless path (`--dump-frame`, tests)
//! run identical logic. Without it, ignore rules would silently apply in one and not the
//! other, and the snapshot tests would be asserting something the app never does.

use std::path::Path;

use crate::ignore_rules::{IgnoreOptions, IgnoreRules};
use crate::service::{DirEntryInfo, FileSystemService, FsResult};

/// Entries materialised per directory level before we stop and summarise.
///
/// One pathological directory (a cache with 200k files) must not stall a render or
/// balloon the tree (ADR-0005 §6).
pub const MAX_ENTRIES_PER_DIR: usize = 10_000;

/// Lists directories on behalf of the tree, applying ignore rules as it goes.
pub struct DirReader<'a> {
    fs: &'a dyn FileSystemService,
    rules: IgnoreRules,
}

impl<'a> DirReader<'a> {
    pub fn new(fs: &'a dyn FileSystemService, root: &Path, options: IgnoreOptions) -> Self {
        let rules = IgnoreRules::for_root(fs, root, options);
        Self { fs, rules }
    }

    /// A reader that hides nothing.
    pub fn unfiltered(fs: &'a dyn FileSystemService) -> Self {
        Self { fs, rules: IgnoreRules::disabled() }
    }

    pub fn rules(&self) -> &IgnoreRules {
        &self.rules
    }

    /// The underlying service, for callers that need to mutate as well as list.
    pub fn service(&self) -> &dyn FileSystemService {
        self.fs
    }

    /// List one level: the service's entries, minus anything ignored, capped.
    ///
    /// Picks up `path`'s own `.gitignore` first, so nesting is honoured lazily — we only
    /// pay for the rules of directories the user actually opened.
    pub fn read(&mut self, path: &Path) -> FsResult<Vec<DirEntryInfo>> {
        self.rules.load_dir(self.fs, path);
        let entries = self.fs.read_dir(path)?;
        let mut kept = self.rules.filter(entries);
        kept.truncate(MAX_ENTRIES_PER_DIR);
        Ok(kept)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    use crate::service::EntryKind;
    use termesh_core::{FsError, FsResult};

    struct Fs {
        entries: Vec<DirEntryInfo>,
        gitignore: Option<&'static str>,
    }

    impl FileSystemService for Fs {
        fn read_dir(&self, _: &Path) -> FsResult<Vec<DirEntryInfo>> {
            Ok(self.entries.clone())
        }
        fn read_file(&self, path: &Path) -> FsResult<Vec<u8>> {
            match (self.gitignore, path.file_name().and_then(|n| n.to_str())) {
                (Some(c), Some(".gitignore")) => Ok(c.as_bytes().to_vec()),
                _ => Err(FsError::NotFound(path.to_path_buf())),
            }
        }
        fn create_file(&self, _: &Path) -> FsResult<()> {
            Ok(())
        }
        fn write_file(&self, _: &Path, _: &[u8]) -> FsResult<()> {
            Ok(())
        }
        fn create_dir(&self, _: &Path) -> FsResult<()> {
            Ok(())
        }
        fn rename(&self, _: &Path, _: &Path) -> FsResult<()> {
            Ok(())
        }
        fn remove_file(&self, _: &Path) -> FsResult<()> {
            Ok(())
        }
        fn remove_dir_all(&self, _: &Path) -> FsResult<()> {
            Ok(())
        }
        fn canonicalize(&self, p: &Path) -> FsResult<PathBuf> {
            Ok(p.to_path_buf())
        }
    }

    fn entry(name: &str, kind: EntryKind) -> DirEntryInfo {
        DirEntryInfo { name: name.into(), path: PathBuf::from("/r").join(name), kind }
    }

    #[test]
    fn ignored_and_hidden_entries_are_filtered_out() {
        let fs = Fs {
            entries: vec![
                entry("src", EntryKind::Dir),
                entry("target", EntryKind::Dir),
                entry(".git", EntryKind::Dir),
                entry("README.md", EntryKind::File),
            ],
            gitignore: Some("target\n"),
        };
        let mut reader = DirReader::new(&fs, Path::new("/r"), IgnoreOptions::default());
        let names: Vec<String> = reader
            .read(Path::new("/r"))
            .unwrap()
            .iter()
            .map(|e| e.name.to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["src", "README.md"]);
    }

    #[test]
    fn an_unfiltered_reader_keeps_everything() {
        let fs = Fs {
            entries: vec![entry(".git", EntryKind::Dir), entry("target", EntryKind::Dir)],
            gitignore: Some("target\n"),
        };
        let mut reader = DirReader::unfiltered(&fs);
        assert_eq!(reader.read(Path::new("/r")).unwrap().len(), 2);
    }

    #[test]
    fn oversized_directories_are_capped() {
        let entries =
            (0..MAX_ENTRIES_PER_DIR + 500).map(|i| entry(&format!("f{i}"), EntryKind::File));
        let fs = Fs { entries: entries.collect(), gitignore: None };
        let mut reader = DirReader::unfiltered(&fs);
        assert_eq!(reader.read(Path::new("/r")).unwrap().len(), MAX_ENTRIES_PER_DIR);
    }

    #[test]
    fn read_errors_propagate_rather_than_becoming_an_empty_listing() {
        struct Denied;
        impl FileSystemService for Denied {
            fn read_dir(&self, p: &Path) -> FsResult<Vec<DirEntryInfo>> {
                Err(FsError::PermissionDenied(p.to_path_buf()))
            }
            fn read_file(&self, p: &Path) -> FsResult<Vec<u8>> {
                Err(FsError::NotFound(p.to_path_buf()))
            }
            fn create_file(&self, _: &Path) -> FsResult<()> {
                Ok(())
            }
            fn write_file(&self, _: &Path, _: &[u8]) -> FsResult<()> {
                Ok(())
            }
            fn create_dir(&self, _: &Path) -> FsResult<()> {
                Ok(())
            }
            fn rename(&self, _: &Path, _: &Path) -> FsResult<()> {
                Ok(())
            }
            fn remove_file(&self, _: &Path) -> FsResult<()> {
                Ok(())
            }
            fn remove_dir_all(&self, _: &Path) -> FsResult<()> {
                Ok(())
            }
            fn canonicalize(&self, p: &Path) -> FsResult<PathBuf> {
                Ok(p.to_path_buf())
            }
        }
        let mut reader = DirReader::unfiltered(&Denied);
        assert!(reader.read(Path::new("/r")).is_err(), "an empty list would look like success");
    }
}
