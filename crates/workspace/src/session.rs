//! Session persistence: recent workspaces, and which one to reopen (ARCHITECTURE.md §13,
//! §16 Phase 02).
//!
//! Reads and writes through [`FileSystemService`] like everything else, so the whole
//! thing is testable against the in-memory fake and the service boundary holds.
//!
//! Phase 02 persisted only the workspace roots. Phase 10 added the rest of what §23
//! item 10 asks for — open buffers, the active tab, pane geometry, and terminal working
//! directories — as a best-effort [`RestoredWorkspace`] alongside the MRU list.
//!
//! The agent session is the one piece that does **not** persist, and that is a protocol
//! limit rather than a gap here: this client has no `session/load`, so a restored
//! workspace starts a fresh session and keeps the prior transcript as read-only history
//! (ADR-0014 §4, `docs/support.md`).
//!
//! The format is a table, and unknown keys written by a newer build survive a
//! round-trip, so adding keys does not break old files in either direction.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use termesh_filesystem::{FileSystemService, FsError};

/// How many recent workspaces to remember.
const MAX_RECENT: usize = 20;

/// What survives a restart.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Session {
    /// The schema version this build writes and fully understands (ADR-0014 §2). No
    /// transitions exist yet — a corrupt or unreadable file already falls back to
    /// `Session::default()` above the version check, so there is nothing to migrate in
    /// memory until the first breaking change gives this field work to do.
    pub version: u32,
    /// Most recently opened first.
    pub recent: Vec<PathBuf>,
    /// The one workspace that was open at clean shutdown. `recent` remains the MRU list;
    /// this is the richer, best-effort state used only when reopening that workspace.
    pub workspace: Option<RestoredWorkspace>,
    /// Fields written by a newer build. They remain attached to the session so the next
    /// legitimate save (for example, recording a newly opened workspace) cannot erase
    /// data this build does not understand (ADR-0014 §2).
    #[serde(flatten)]
    unknown: BTreeMap<String, toml::Value>,
}

impl Default for Session {
    fn default() -> Self {
        Self {
            version: Session::CURRENT_VERSION,
            recent: Vec::new(),
            workspace: None,
            unknown: BTreeMap::new(),
        }
    }
}

/// Persisted pane percentages. Kept in the workspace crate so session persistence does
/// not make the state crate depend on the UI crate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PaneGeometry {
    pub sidebar_pct: u16,
    pub bottom_pct: u16,
    pub agent_pct: u16,
    #[serde(flatten)]
    unknown: BTreeMap<String, toml::Value>,
}

impl Default for PaneGeometry {
    fn default() -> Self {
        Self { sidebar_pct: 22, bottom_pct: 32, agent_pct: 26, unknown: BTreeMap::new() }
    }
}

impl PaneGeometry {
    pub fn new(sidebar_pct: u16, bottom_pct: u16, agent_pct: u16) -> Self {
        Self { sidebar_pct, bottom_pct, agent_pct, unknown: BTreeMap::new() }
    }

    pub fn set_percentages(&mut self, sidebar_pct: u16, bottom_pct: u16, agent_pct: u16) {
        self.sidebar_pct = sidebar_pct;
        self.bottom_pct = bottom_pct;
        self.agent_pct = agent_pct;
    }
}

/// Speaker identity for transcript history persisted independently of ACP wire state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentHistorySpeaker {
    You,
    Agent,
    Thought,
}

/// One display-only line from the prior agent session. It is never replayed to ACP.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentHistoryLine {
    pub speaker: AgentHistorySpeaker,
    pub text: String,
}

/// Workspace-owned state that can be reconstructed without pretending to resume an OS
/// process or ACP session (ADR-0014 §4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RestoredWorkspace {
    pub root: PathBuf,
    pub open: Vec<PathBuf>,
    pub active: Option<PathBuf>,
    pub layout: PaneGeometry,
    /// Working directories only. Restore starts fresh shells in these directories.
    pub terminals: Vec<PathBuf>,
    /// Read-only display history. It is deliberately separate from a live ACP session.
    pub agent_history: Vec<AgentHistoryLine>,
    /// Preserve fields written by a newer build when this build next saves the session.
    #[serde(flatten)]
    unknown: BTreeMap<String, toml::Value>,
}

impl Default for RestoredWorkspace {
    fn default() -> Self {
        Self {
            root: PathBuf::new(),
            open: Vec::new(),
            active: None,
            layout: PaneGeometry::default(),
            terminals: Vec::new(),
            agent_history: Vec::new(),
            unknown: BTreeMap::new(),
        }
    }
}

impl RestoredWorkspace {
    pub fn new(root: PathBuf) -> Self {
        Self { root, ..Self::default() }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionDiagnostic {
    pub problem: String,
    pub fallback: String,
}

impl Session {
    pub const CURRENT_VERSION: u32 = 1;

    /// Parse and migrate in memory without ever rewriting as a side effect of loading.
    /// Unknown fields stay in `unknown`, so a later real save preserves them verbatim.
    pub fn parse(text: &str) -> (Self, Vec<SessionDiagnostic>) {
        let mut session: Session = match toml::from_str(text) {
            Ok(session) => session,
            Err(error) => {
                return (
                    Session::default(),
                    vec![SessionDiagnostic {
                        problem: error.to_string(),
                        fallback: "using an empty session".into(),
                    }],
                );
            }
        };
        let mut diagnostics = Vec::new();

        if session.version > Self::CURRENT_VERSION {
            diagnostics.push(SessionDiagnostic {
                problem: format!(
                    "version {} is newer than this build understands (current: {})",
                    session.version,
                    Self::CURRENT_VERSION
                ),
                fallback: "loaded what was understood and preserved newer fields".into(),
            });
        } else if session.version < Self::CURRENT_VERSION {
            // There are no transitions yet; stamping the current version is the complete
            // v0 -> v1 in-memory migration and is idempotent.
            session.version = Self::CURRENT_VERSION;
        }

        for key in session.unknown.keys() {
            diagnostics.push(SessionDiagnostic {
                problem: format!("unknown session key '{key}'"),
                fallback: "preserving it unchanged".into(),
            });
        }
        if let Some(workspace) = &session.workspace {
            for key in workspace.unknown.keys() {
                diagnostics.push(SessionDiagnostic {
                    problem: format!("unknown session key 'workspace.{key}'"),
                    fallback: "preserving it unchanged".into(),
                });
            }
            for key in workspace.layout.unknown.keys() {
                diagnostics.push(SessionDiagnostic {
                    problem: format!("unknown session key 'workspace.layout.{key}'"),
                    fallback: "preserving it unchanged".into(),
                });
            }
        }

        (session, diagnostics)
    }

    /// The workspace to reopen when started with no path.
    pub fn last_root(&self) -> Option<&Path> {
        self.recent.first().map(PathBuf::as_path)
    }

    /// Record a workspace as most-recently-used, de-duplicating and capping the list.
    pub fn record(&mut self, root: &Path) {
        self.recent.retain(|p| p != root);
        self.recent.insert(0, root.to_path_buf());
        self.recent.truncate(MAX_RECENT);
    }

    /// Drop entries that no longer exist, so a deleted project stops being offered.
    pub fn prune_missing(&mut self, fs: &dyn FileSystemService) {
        self.recent.retain(|p| fs.read_dir(p).is_ok());
        if self.workspace.as_ref().is_some_and(|workspace| fs.read_dir(&workspace.root).is_err()) {
            self.workspace = None;
        }
    }
}

/// Service boundary: persist and restore workspace sessions.
/// Widgets and the agent go through this trait, never the OS directly (ARCHITECTURE.md §7.4).
pub trait SessionStore {
    /// Load the stored session. A missing or corrupt file yields a default session —
    /// losing session state must never stop the editor from starting.
    fn load(&self) -> Session;

    /// Load with the degradation details needed by the application status surface.
    /// In-memory stores have no parsing boundary, so their default has no diagnostics.
    fn load_with_diagnostics(&self) -> (Session, Vec<SessionDiagnostic>) {
        (self.load(), Vec::new())
    }

    /// Persist the session. Returns the error rather than panicking; failing to save
    /// recents is a nuisance, not a crash.
    fn save(&self, session: &Session) -> Result<(), FsError>;
}

/// Stores the session as TOML at a fixed path, through the filesystem service.
pub struct FileSessionStore<'a> {
    fs: &'a dyn FileSystemService,
    path: PathBuf,
}

impl<'a> FileSessionStore<'a> {
    pub fn new(fs: &'a dyn FileSystemService, path: impl Into<PathBuf>) -> Self {
        Self { fs, path: path.into() }
    }
}

impl SessionStore for FileSessionStore<'_> {
    fn load(&self) -> Session {
        self.load_with_diagnostics().0
    }

    fn load_with_diagnostics(&self) -> (Session, Vec<SessionDiagnostic>) {
        let bytes = match self.fs.read_file(&self.path) {
            Ok(bytes) => bytes,
            Err(FsError::NotFound(_)) => return (Session::default(), Vec::new()),
            Err(error) => {
                return (
                    Session::default(),
                    vec![SessionDiagnostic {
                        problem: error.to_string(),
                        fallback: "using an empty session".into(),
                    }],
                );
            }
        };
        let text = match String::from_utf8(bytes) {
            Ok(text) => text,
            Err(_) => {
                return (
                    Session::default(),
                    vec![SessionDiagnostic {
                        problem: "session file is not valid UTF-8".into(),
                        fallback: "using an empty session".into(),
                    }],
                );
            }
        };
        Session::parse(&text)
    }

    fn save(&self, session: &Session) -> Result<(), FsError> {
        let text = toml::to_string_pretty(session)
            .map_err(|e| FsError::Other { path: self.path.clone(), message: e.to_string() })?;

        if let Some(parent) = self.path.parent() {
            self.fs.create_dir(parent)?;
        }
        // `create_file` refuses to clobber, so replace rather than overwrite in place.
        let _ = self.fs.remove_file(&self.path);
        self.fs.create_file(&self.path)?;
        self.fs.write_file(&self.path, text.as_bytes())
    }
}

/// A store that keeps the session in memory. For tests, and for running with no home
/// directory — persistence is a convenience, not a requirement.
#[derive(Debug, Default)]
pub struct MemorySessionStore {
    session: std::sync::Mutex<Session>,
}

impl MemorySessionStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl SessionStore for MemorySessionStore {
    fn load(&self) -> Session {
        self.session.lock().unwrap().clone()
    }
    fn save(&self, session: &Session) -> Result<(), FsError> {
        *self.session.lock().unwrap() = session.clone();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use termesh_test_support::FakeFileSystem;

    fn store(fs: &FakeFileSystem) -> FileSessionStore<'_> {
        FileSessionStore::new(fs, "/cfg/termesh/session.toml")
    }

    #[test]
    fn the_most_recent_workspace_comes_first() {
        let mut s = Session::default();
        s.record(Path::new("/a"));
        s.record(Path::new("/b"));
        assert_eq!(s.last_root(), Some(Path::new("/b")));
        assert_eq!(s.recent, [PathBuf::from("/b"), PathBuf::from("/a")]);
    }

    #[test]
    fn reopening_a_workspace_moves_it_to_the_front_without_duplicating() {
        let mut s = Session::default();
        s.record(Path::new("/a"));
        s.record(Path::new("/b"));
        s.record(Path::new("/a"));
        assert_eq!(s.recent, [PathBuf::from("/a"), PathBuf::from("/b")]);
    }

    #[test]
    fn the_recent_list_is_capped() {
        let mut s = Session::default();
        for i in 0..MAX_RECENT + 10 {
            s.record(Path::new(&format!("/p{i}")));
        }
        assert_eq!(s.recent.len(), MAX_RECENT);
        assert_eq!(s.last_root(), Some(Path::new(&format!("/p{}", MAX_RECENT + 9))));
    }

    #[test]
    fn an_empty_session_has_nothing_to_reopen() {
        assert_eq!(Session::default().last_root(), None);
    }

    #[test]
    fn a_session_round_trips_through_the_file() {
        let fs = FakeFileSystem::new();
        fs.add_dir("/proj");
        let mut s = Session::default();
        s.record(Path::new("/proj"));

        store(&fs).save(&s).unwrap();
        assert_eq!(store(&fs).load(), s);
    }

    #[test]
    fn restored_workspace_state_round_trips_without_losing_restart_owned_fields() {
        let fs = FakeFileSystem::new();
        let session = Session {
            workspace: Some(RestoredWorkspace {
                root: PathBuf::from("/proj"),
                open: vec![PathBuf::from("/proj/src/main.rs"), PathBuf::from("/proj/src/lib.rs")],
                active: Some(PathBuf::from("/proj/src/lib.rs")),
                layout: PaneGeometry::new(28, 35, 24),
                terminals: vec![PathBuf::from("/proj"), PathBuf::from("/proj/src")],
                agent_history: vec![AgentHistoryLine {
                    speaker: AgentHistorySpeaker::Agent,
                    text: "Prior answer".into(),
                }],
                unknown: BTreeMap::new(),
            }),
            ..Session::default()
        };

        store(&fs).save(&session).unwrap();

        assert_eq!(store(&fs).load(), session);
    }

    #[test]
    fn saving_twice_replaces_rather_than_appends() {
        let fs = FakeFileSystem::new();
        let mut s = Session::default();
        s.record(Path::new("/a"));
        store(&fs).save(&s).unwrap();

        s.record(Path::new("/b"));
        store(&fs).save(&s).unwrap();

        assert_eq!(store(&fs).load().recent, [PathBuf::from("/b"), PathBuf::from("/a")]);
    }

    #[test]
    fn a_missing_session_file_loads_as_empty() {
        let fs = FakeFileSystem::new();
        assert_eq!(store(&fs).load(), Session::default());
    }

    #[test]
    fn a_corrupt_session_file_loads_as_empty_rather_than_failing() {
        // Losing recents must never stop the editor from starting.
        let fs = FakeFileSystem::new();
        fs.add_file("/cfg/termesh/session.toml", b"this is not valid toml {{{");
        assert_eq!(store(&fs).load(), Session::default());
    }

    #[test]
    fn a_session_file_with_no_version_key_is_treated_as_current() {
        let fs = FakeFileSystem::new();
        fs.add_file("/cfg/termesh/session.toml", b"recent = []\n");
        assert_eq!(store(&fs).load().version, Session::CURRENT_VERSION);
    }

    #[test]
    fn loading_a_session_does_not_rewrite_the_file() {
        let fs = FakeFileSystem::new();
        fs.add_file("/cfg/termesh/session.toml", b"# my note\nrecent = []\n");
        let _ = store(&fs).load();
        assert_eq!(
            fs.read_file(Path::new("/cfg/termesh/session.toml")).unwrap(),
            b"# my note\nrecent = []\n".to_vec()
        );
    }

    #[test]
    fn a_future_session_loads_known_fields_and_reports_the_fallback() {
        let fs = FakeFileSystem::new();
        fs.add_file(
            "/cfg/termesh/session.toml",
            format!("version = {}\nrecent = [\"/proj\"]\n", Session::CURRENT_VERSION + 1)
                .as_bytes(),
        );

        let (session, diagnostics) = store(&fs).load_with_diagnostics();

        assert_eq!(session.recent, [PathBuf::from("/proj")]);
        assert_eq!(diagnostics.len(), 1);
        assert!(diagnostics[0].problem.contains("newer"));
        assert!(diagnostics[0].fallback.contains("understood"));
    }

    #[test]
    fn an_unknown_session_key_is_reported_and_preserved_on_the_next_real_write() {
        let fs = FakeFileSystem::new();
        fs.add_file(
            "/cfg/termesh/session.toml",
            b"version = 1\nrecent = []\nfuture_layout = \"keep me\"\n",
        );

        let (mut session, diagnostics) = store(&fs).load_with_diagnostics();
        assert_eq!(diagnostics.len(), 1);
        assert!(diagnostics[0].problem.contains("future_layout"));

        session.record(Path::new("/proj"));
        store(&fs).save(&session).unwrap();
        let saved =
            String::from_utf8(fs.read_file(Path::new("/cfg/termesh/session.toml")).unwrap())
                .unwrap();
        assert!(saved.contains("future_layout = \"keep me\""), "{saved}");
    }

    #[test]
    fn unknown_nested_workspace_keys_are_reported_and_preserved() {
        let fs = FakeFileSystem::new();
        fs.add_file(
            "/cfg/termesh/session.toml",
            br#"version = 1
recent = []

[workspace]
root = "/proj"
future_workspace = "keep workspace"

[workspace.layout]
sidebar_pct = 22
bottom_pct = 32
agent_pct = 26
future_layout = "keep layout"
"#,
        );

        let (session, diagnostics) = store(&fs).load_with_diagnostics();
        assert_eq!(diagnostics.len(), 2, "{diagnostics:#?}");
        assert!(diagnostics.iter().any(|item| item.problem.contains("future_workspace")));
        assert!(diagnostics.iter().any(|item| item.problem.contains("future_layout")));

        store(&fs).save(&session).unwrap();
        let saved =
            String::from_utf8(fs.read_file(Path::new("/cfg/termesh/session.toml")).unwrap())
                .unwrap();
        assert!(saved.contains("future_workspace = \"keep workspace\""), "{saved}");
        assert!(saved.contains("future_layout = \"keep layout\""), "{saved}");
    }

    #[test]
    fn missing_workspaces_are_pruned() {
        let fs = FakeFileSystem::new();
        fs.add_dir("/still/here");
        let mut s = Session::default();
        s.record(Path::new("/deleted"));
        s.record(Path::new("/still/here"));

        s.prune_missing(&fs);
        assert_eq!(s.recent, [PathBuf::from("/still/here")]);
    }

    #[test]
    fn a_missing_restored_workspace_is_pruned_without_costing_valid_recents() {
        let fs = FakeFileSystem::new();
        fs.add_dir("/still/here");
        let mut session = Session {
            recent: vec![PathBuf::from("/deleted"), PathBuf::from("/still/here")],
            workspace: Some(RestoredWorkspace::new(PathBuf::from("/deleted"))),
            ..Session::default()
        };

        session.prune_missing(&fs);

        assert!(session.workspace.is_none());
        assert_eq!(session.recent, [PathBuf::from("/still/here")]);
    }

    #[test]
    fn the_memory_store_round_trips() {
        let store = MemorySessionStore::new();
        let mut s = Session::default();
        s.record(Path::new("/x"));
        store.save(&s).unwrap();
        assert_eq!(store.load(), s);
    }
}
