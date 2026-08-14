//! Project roots, sessions, settings, project-type detection. Phase 02.
//!
//! See ARCHITECTURE.md §7 for how this crate fits the workspace.
#![forbid(unsafe_code)]

pub mod drafts;
pub mod language;
pub mod permissions;
pub mod root;
pub mod session;
pub mod snapshot;

pub use language::{LanguageSettings, LanguageSettingsError};
pub use permissions::{CommandGrant, FilePermissionStore, PermissionPolicy, PermissionStore};
pub use root::{detect_root, kind_labels, project_kind_of, ProjectKind, WorkspaceRoot};
pub use session::{
    AgentHistoryLine, AgentHistorySpeaker, FileSessionStore, MemorySessionStore, PaneGeometry,
    RestoredWorkspace, Session, SessionDiagnostic, SessionStore,
};
pub use snapshot::{TreeEntry, WorkspaceSnapshot};
