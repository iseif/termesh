//! Ignore semantics for the explorer (ADR-0005 §4).
//!
//! Uses ripgrep's `ignore` crate for the matching itself — `.gitignore` precedence is a
//! known tar pit, and the explorer and the content search share this matcher so the two
//! agree on what exists. A file the tree hides but search finds is a bug report.
//!
//! The ignore *files* are read through [`FileSystemService`] rather than by the crate's
//! own I/O, which keeps the service boundary intact (CONTRIBUTING.md invariants) and — more
//! usefully — makes all of this testable against the in-memory fake.

use std::path::{Path, PathBuf};

use ignore::gitignore::{Gitignore, GitignoreBuilder};

use crate::service::{DirEntryInfo, EntryKind, FileSystemService};

/// What the explorer shows. Both default to off: the default view should look like the
/// project, and the agent's context should not be full of `target/` and `node_modules/`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IgnoreOptions {
    /// Show entries matched by an ignore file (rendered dimmed by the widget).
    pub show_ignored: bool,
    /// Show dotfiles.
    pub show_hidden: bool,
}

impl IgnoreOptions {
    /// Show everything — useful for "reveal in explorer" and for tests.
    pub fn show_all() -> Self {
        Self { show_ignored: true, show_hidden: true }
    }
}

/// The ignore-file names we honour, in the order git itself applies them.
const IGNORE_FILES: &[&str] = &[".gitignore", ".ignore"];

/// A chain of matchers, one per directory that contributed rules. Deeper directories win,
/// which is what `.gitignore` nesting means.
pub struct IgnoreRules {
    matchers: Vec<(PathBuf, Gitignore)>,
    options: IgnoreOptions,
    root: PathBuf,
}

impl IgnoreRules {
    /// Build the root-level rules, reading ignore files through `fs`.
    pub fn for_root(fs: &dyn FileSystemService, root: &Path, options: IgnoreOptions) -> Self {
        let mut rules = Self { matchers: Vec::new(), options, root: root.to_path_buf() };
        rules.load_dir(fs, root);
        // git's own repo-local excludes, which live outside the working tree.
        rules.load_file(fs, root, &root.join(".git/info/exclude"));
        rules
    }

    /// No rules at all — every entry is visible.
    pub fn disabled() -> Self {
        Self { matchers: Vec::new(), options: IgnoreOptions::show_all(), root: PathBuf::new() }
    }

    pub fn options(&self) -> IgnoreOptions {
        self.options
    }

    /// Read `dir`'s own ignore files, if it has any we have not already loaded.
    ///
    /// Called before listing each directory, so nesting is honoured lazily: we only pay
    /// for the rules of directories the user actually opened.
    pub fn load_dir(&mut self, fs: &dyn FileSystemService, dir: &Path) {
        if self.matchers.iter().any(|(d, _)| d == dir) {
            return;
        }
        for name in IGNORE_FILES {
            let path = dir.join(name);
            self.load_file(fs, dir, &path);
        }
    }

    fn load_file(&mut self, fs: &dyn FileSystemService, anchor: &Path, path: &Path) {
        // A missing ignore file is the normal case, not an error worth surfacing.
        let Ok(bytes) = fs.read_file(path) else { return };
        let Ok(text) = String::from_utf8(bytes) else { return };

        let mut builder = GitignoreBuilder::new(anchor);
        let mut added = false;
        for line in text.lines() {
            // A malformed glob should cost us that line, not the whole file.
            if builder.add_line(None, line).is_ok() {
                added = true;
            }
        }
        if !added {
            return;
        }
        if let Ok(matcher) = builder.build() {
            self.matchers.push((anchor.to_path_buf(), matcher));
        }
    }

    /// Whether `path` should be hidden from the explorer.
    pub fn is_hidden(&self, path: &Path, is_dir: bool) -> bool {
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();

        // `.git` is machinery, never content. Hidden unless dotfiles are shown.
        if !self.options.show_hidden && name.starts_with('.') {
            return true;
        }
        if self.options.show_ignored {
            return false;
        }
        self.is_ignored(path, is_dir)
    }

    /// Whether an ignore file matches `path`. Deepest matcher wins; an explicit
    /// whitelist (`!pattern`) at that depth un-ignores it.
    pub fn is_ignored(&self, path: &Path, is_dir: bool) -> bool {
        // Deepest anchor first, so a nested .gitignore overrides the root's.
        let mut candidates: Vec<&(PathBuf, Gitignore)> =
            self.matchers.iter().filter(|(dir, _)| path.starts_with(dir)).collect();
        candidates.sort_by_key(|(dir, _)| std::cmp::Reverse(dir.components().count()));

        for (_, matcher) in candidates {
            // `matched` tests only the path itself, so a rule naming a directory —
            // `target` — hid `target` and nothing beneath it. The tree never noticed,
            // because it asks about a directory before descending and stops there. The
            // watcher did: the OS hands it deep paths, so every file cargo wrote under
            // `target` looked like a real change, reached the language server as a
            // watched-file notification, and made rust-analyzer re-analyse — which runs
            // cargo check, which writes to `target` again. The `starts_with` filter above
            // guarantees the path is under this matcher's root, which this call requires.
            let m = matcher.matched_path_or_any_parents(path, is_dir);
            if m.is_ignore() {
                return true;
            }
            if m.is_whitelist() {
                return false;
            }
        }
        false
    }

    /// Drop the entries the explorer should not show.
    pub fn filter(&self, entries: Vec<DirEntryInfo>) -> Vec<DirEntryInfo> {
        entries.into_iter().filter(|e| !self.is_hidden(&e.path, e.kind == EntryKind::Dir)).collect()
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
}

/// Whether `path` matches one of `patterns` — literal globs from `config.toml`'s
/// `exclusions` key (ADR-0014 Task 3), never a `.gitignore` file.
///
/// A one-off matcher, not a field on [`IgnoreRules`]: the pattern list is short and
/// changes only when the user edits their config, so recompiling per call trades a
/// negligible cost for never going stale after a `config.reload` (Task 5) — no cache to
/// invalidate. `patterns` is anchored at `root`, exactly as a root-level `.gitignore`
/// would be.
pub fn matches_exclusion(root: &Path, patterns: &[String], path: &Path, is_dir: bool) -> bool {
    if patterns.is_empty() {
        return false;
    }
    let mut builder = GitignoreBuilder::new(root);
    for pattern in patterns {
        // A malformed glob should cost the user that one pattern, not the whole list.
        let _ = builder.add_line(None, pattern);
    }
    let Ok(matcher) = builder.build() else { return false };
    matcher.matched(path, is_dir).is_ignore()
}

impl std::fmt::Debug for IgnoreRules {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IgnoreRules")
            .field("root", &self.root)
            .field("options", &self.options)
            .field("matchers", &self.matchers.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    use termesh_core::{FsError, FsResult};

    /// Minimal in-crate fake: `test-support`'s richer one depends on this crate.
    #[derive(Default)]
    struct Files(Vec<(PathBuf, Vec<u8>)>);

    impl Files {
        fn with(pairs: &[(&str, &str)]) -> Self {
            Self(pairs.iter().map(|(p, c)| (PathBuf::from(p), c.as_bytes().to_vec())).collect())
        }
    }

    impl FileSystemService for Files {
        fn read_file(&self, path: &Path) -> FsResult<Vec<u8>> {
            self.0
                .iter()
                .find(|(p, _)| p == path)
                .map(|(_, c)| c.clone())
                .ok_or_else(|| FsError::NotFound(path.to_path_buf()))
        }
        fn read_dir(&self, _: &Path) -> FsResult<Vec<DirEntryInfo>> {
            Ok(Vec::new())
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

    fn rules(files: &[(&str, &str)], options: IgnoreOptions) -> IgnoreRules {
        IgnoreRules::for_root(&Files::with(files), Path::new("/r"), options)
    }

    fn default_rules(files: &[(&str, &str)]) -> IgnoreRules {
        rules(files, IgnoreOptions::default())
    }

    /// A `.gitignore` naming a directory has to hide everything under it, not just the
    /// directory entry. The tree never noticed, because it asks about `target` before
    /// descending and stops there — but the file watcher is handed deep paths straight
    /// from the OS, so `target/debug/deps/x.rlib` was reported as a real change. On a
    /// Rust project that is thousands of events per build, forwarded to the language
    /// server as watched-file changes, which makes rust-analyzer re-analyse, which runs
    /// cargo check, which writes to `target` again.
    #[test]
    fn an_ignored_directory_hides_the_files_underneath_it() {
        let rules = default_rules(&[("/r/.gitignore", "target\n")]);

        assert!(rules.is_hidden(Path::new("/r/target"), true), "the directory itself");
        assert!(rules.is_hidden(Path::new("/r/target/debug"), true), "a directory inside it");
        assert!(
            rules.is_hidden(Path::new("/r/target/debug/deps/orders.rlib"), false),
            "a file several levels down"
        );
        assert!(!rules.is_hidden(Path::new("/r/src/main.rs"), false), "and nothing else");
    }

    #[test]
    fn a_configured_exclusion_matches_like_a_gitignore_pattern() {
        assert!(matches_exclusion(
            Path::new("/r"),
            &["*.log".to_string()],
            Path::new("/r/debug.log"),
            false,
        ));
        assert!(!matches_exclusion(
            Path::new("/r"),
            &["*.log".to_string()],
            Path::new("/r/src"),
            true,
        ));
    }

    #[test]
    fn no_patterns_excludes_nothing() {
        assert!(!matches_exclusion(Path::new("/r"), &[], Path::new("/r/anything"), false));
    }

    #[test]
    fn a_malformed_pattern_costs_only_itself() {
        let patterns = vec!["[".to_string(), "*.log".to_string()];
        assert!(matches_exclusion(Path::new("/r"), &patterns, Path::new("/r/debug.log"), false));
    }

    #[test]
    fn gitignore_patterns_hide_matching_entries() {
        let r = default_rules(&[("/r/.gitignore", "target\nnode_modules\n*.log\n")]);
        assert!(r.is_hidden(Path::new("/r/target"), true));
        assert!(r.is_hidden(Path::new("/r/node_modules"), true));
        assert!(r.is_hidden(Path::new("/r/debug.log"), false));
        assert!(!r.is_hidden(Path::new("/r/src"), true));
    }

    #[test]
    fn dotfiles_are_hidden_by_default() {
        let r = default_rules(&[]);
        assert!(r.is_hidden(Path::new("/r/.git"), true));
        assert!(r.is_hidden(Path::new("/r/.env"), false));
        assert!(!r.is_hidden(Path::new("/r/README.md"), false));
    }

    #[test]
    fn show_hidden_reveals_dotfiles_but_still_honours_ignore_files() {
        let r = rules(
            &[("/r/.gitignore", "target\n")],
            IgnoreOptions { show_hidden: true, show_ignored: false },
        );
        assert!(!r.is_hidden(Path::new("/r/.env"), false), "dotfile now visible");
        assert!(r.is_hidden(Path::new("/r/target"), true), "ignore rules still apply");
    }

    #[test]
    fn show_ignored_reveals_ignored_entries() {
        let r = rules(
            &[("/r/.gitignore", "target\n")],
            IgnoreOptions { show_ignored: true, show_hidden: true },
        );
        assert!(!r.is_hidden(Path::new("/r/target"), true));
    }

    #[test]
    fn whitelist_patterns_un_ignore() {
        let r = default_rules(&[("/r/.gitignore", "*.log\n!keep.log\n")]);
        assert!(r.is_hidden(Path::new("/r/debug.log"), false));
        assert!(!r.is_hidden(Path::new("/r/keep.log"), false), "! should win");
    }

    #[test]
    fn dot_ignore_files_are_honoured_alongside_gitignore() {
        let r = default_rules(&[("/r/.ignore", "secrets\n")]);
        assert!(r.is_hidden(Path::new("/r/secrets"), true));
    }

    #[test]
    fn git_info_exclude_is_honoured() {
        let r = default_rules(&[("/r/.git/info/exclude", "scratch\n")]);
        assert!(r.is_hidden(Path::new("/r/scratch"), true));
    }

    #[test]
    fn a_nested_gitignore_overrides_the_root() {
        let fs = Files::with(&[("/r/.gitignore", "*.log\n"), ("/r/logs/.gitignore", "!*.log\n")]);
        let mut r = IgnoreRules::for_root(&fs, Path::new("/r"), IgnoreOptions::default());
        r.load_dir(&fs, Path::new("/r/logs"));

        assert!(r.is_hidden(Path::new("/r/debug.log"), false), "root rule still applies");
        assert!(!r.is_hidden(Path::new("/r/logs/debug.log"), false), "the deeper .gitignore wins");
    }

    #[test]
    fn a_missing_gitignore_is_not_an_error() {
        let r = default_rules(&[]);
        assert!(!r.is_hidden(Path::new("/r/anything.txt"), false));
    }

    #[test]
    fn comments_and_blank_lines_are_ignored() {
        let r = default_rules(&[("/r/.gitignore", "# a comment\n\n  \ntarget\n")]);
        assert!(r.is_hidden(Path::new("/r/target"), true));
        assert!(!r.is_hidden(Path::new("/r/src"), true));
    }

    #[test]
    fn filter_drops_hidden_entries_and_keeps_the_rest() {
        let r = default_rules(&[("/r/.gitignore", "target\n")]);
        let entries = vec![
            DirEntryInfo { name: "src".into(), path: "/r/src".into(), kind: EntryKind::Dir },
            DirEntryInfo { name: "target".into(), path: "/r/target".into(), kind: EntryKind::Dir },
            DirEntryInfo { name: ".git".into(), path: "/r/.git".into(), kind: EntryKind::Dir },
            DirEntryInfo {
                name: "README.md".into(),
                path: "/r/README.md".into(),
                kind: EntryKind::File,
            },
        ];
        let kept: Vec<String> =
            r.filter(entries).iter().map(|e| e.name.to_string_lossy().into_owned()).collect();
        assert_eq!(kept, ["src", "README.md"]);
    }

    #[test]
    fn disabled_rules_show_everything() {
        let r = IgnoreRules::disabled();
        assert!(!r.is_hidden(Path::new("/r/.git"), true));
        assert!(!r.is_hidden(Path::new("/r/target"), true));
    }

    #[test]
    fn a_directory_only_pattern_does_not_hide_a_file_of_the_same_name() {
        let r = default_rules(&[("/r/.gitignore", "build/\n")]);
        assert!(r.is_hidden(Path::new("/r/build"), true), "the directory is ignored");
        assert!(!r.is_hidden(Path::new("/r/build"), false), "a file named build is not");
    }
}
