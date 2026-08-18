//! Shared foundation: stable typed IDs, the action registry, input, commands, errors.
//!
//! The **action registry** here is the keystone of the whole design (ARCHITECTURE.md §3):
//! the *same* named actions back the keymap, the command palette, and the agent's
//! ACP tool surface. Build this well and "agent-native" mostly falls out.
#![forbid(unsafe_code)]

use core::fmt;

pub mod agent;
pub mod fs;
pub mod git;
pub mod input;
pub mod lsp;
pub mod message;
pub mod search;
pub mod task;
pub mod terminal;

pub use agent::{
    AgentCapabilities, AgentEvent, AgentRequest, PermissionDecision, PromptCapabilities,
    ProposedEditDiff, SessionMode, StopReason,
};
pub use fs::{DirEntryInfo, EntryKind, FsError, FsEvent, FsRequest, FsResult};
pub use git::{
    GitBranch, GitBranchStatus, GitChangeKind, GitContextDiff, GitDiffTarget, GitEvent, GitFailure,
    GitFailureKind, GitFileDiff, GitFileStatus, GitOperation, GitRepositorySnapshot, GitRequest,
    GitResult,
};
pub use lsp::{
    CodeAction, CompletionItem, Diagnostic, DiagnosticOrigin, DiagnosticSeverity, DocumentSymbol,
    HoverText, Location, LspEvent, LspFailure, LspFailureKind, LspRequest, LspResult, SymbolKind,
    SymbolLocation, TextChange, TextEdit, TextPosition, TextRange, WatchedFileChange,
    WorkspaceEdit,
};
pub use message::AppMessage;
pub use search::{SearchEvent, SearchMatch, SearchMode, SearchRequest};
pub use task::{Problem, ProblemSeverity, TaskOrigin, TaskSpec, TaskStatus};
pub use terminal::{
    AgentTerminalOperation, AgentTerminalResponse, PtyEvent, PtyRequest, TerminalExit,
    TerminalOwner, TerminalSize, TerminalSpec, TerminalStatus,
};

macro_rules! id_type {
    ($(#[$m:meta])* $name:ident) => {
        $(#[$m])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(pub u64);
        impl $name {
            pub const fn new(v: u64) -> Self { Self(v) }
        }
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, concat!(stringify!($name), "({})"), self.0)
            }
        }
    };
}

id_type!(
    /// A single open project/workspace root.
    WorkspaceId
);
id_type!(PaneId);
id_type!(
    /// A node in the file-explorer tree. Identity is the id, never the path — paths
    /// move under rename and watch events (ARCHITECTURE.md §7.3).
    NodeId
);
id_type!(BufferId);
id_type!(DocumentId);
id_type!(TerminalId);
id_type!(TerminalGeneration);
id_type!(TaskRunId);
id_type!(SearchRequestId);
id_type!(PreviewRequestId);
id_type!(LocationRequestId);
id_type!(GitRequestId);
id_type!(LspServerId);
id_type!(LspRequestId);
id_type!(AgentId);
id_type!(SessionId);
id_type!(TurnId);
id_type!(ProposalId);
id_type!(PermissionRequestId);
id_type!(
    /// One ACP terminal method awaiting a model/service response.
    AgentTerminalRequestId
);
id_type!(
    /// One `fs/read_text_file` call from the agent.
    ///
    /// Correlation is by id, never by path: an agent may read the same file twice in a
    /// turn (read, edit, re-read to confirm), and keying on the path would let the second
    /// call overwrite the first — leaving one of them unanswered and the agent blocked
    /// forever on a reply that never comes.
    ReadRequestId
);

/// A named, invocable action — the shared vocabulary of the keymap, command palette,
/// plugins, and the agent tool schema exposed over ACP (§3, §6.2).
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    FileOpen,
    FileSave,
    FileNew,
    FolderNew,
    FileRename,
    FileDelete,
    WorkspaceSearch,
    WorkspaceRestoreDrafts,
    PaneSplitRight,
    FocusProject,
    FocusEditor,
    FocusAgent,
    TerminalNew,
    TerminalRun,
    TerminalFocus,
    TerminalNext,
    TerminalPrevious,
    TerminalRestart,
    TerminalClose,
    TerminalCopyMode,
    GitShow,
    GitStage,
    GitUnstage,
    GitCommit,
    GitBranchCheckout,
    GitFetch,
    GitPull,
    GitPush,
    TaskRun,
    TaskCancel,
    ProblemsShow,
    ProblemsNext,
    ProblemsPrevious,
    EditorGotoDefinition,
    LspHover,
    LspCompletion,
    LspReferences,
    LspDocumentSymbols,
    LspWorkspaceSymbols,
    LspRename,
    LspCodeAction,
    LspFormat,
    LspRestart,
    EditorApplyTransaction,
    AgentSessionNew,
    AgentPrompt,
    AgentMode,
    AgentProposalAccept,
    AgentProposalReject,
    HelpShow,
    ConfigReload,
}

impl Action {
    /// The stable string id (also the tool name advertised to ACP agents).
    pub fn id(&self) -> &'static str {
        match self {
            Action::FileOpen => "file.open",
            Action::FileSave => "file.save",
            Action::FileNew => "file.new",
            Action::FolderNew => "file.new_folder",
            Action::FileRename => "file.rename",
            Action::FileDelete => "file.delete",
            Action::WorkspaceSearch => "workspace.search",
            Action::WorkspaceRestoreDrafts => "workspace.restore_drafts",
            Action::PaneSplitRight => "pane.split_right",
            Action::FocusProject => "focus.project",
            Action::FocusEditor => "focus.editor",
            Action::FocusAgent => "focus.agent",
            Action::TerminalNew => "terminal.new",
            Action::TerminalRun => "terminal.run",
            Action::TerminalFocus => "terminal.focus",
            Action::TerminalNext => "terminal.next",
            Action::TerminalPrevious => "terminal.previous",
            Action::TerminalRestart => "terminal.restart",
            Action::TerminalClose => "terminal.close",
            Action::TerminalCopyMode => "terminal.copy_mode",
            Action::GitShow => "git.show",
            Action::GitStage => "git.stage",
            Action::GitUnstage => "git.unstage",
            Action::GitCommit => "git.commit",
            Action::GitBranchCheckout => "git.branch.checkout",
            Action::GitFetch => "git.fetch",
            Action::GitPull => "git.pull",
            Action::GitPush => "git.push",
            Action::TaskRun => "task.run",
            Action::TaskCancel => "task.cancel",
            Action::ProblemsShow => "problems.show",
            Action::ProblemsNext => "problems.next",
            Action::ProblemsPrevious => "problems.previous",
            Action::EditorGotoDefinition => "editor.goto_definition",
            Action::LspHover => "lsp.hover",
            Action::LspCompletion => "lsp.completion",
            Action::LspReferences => "lsp.references",
            Action::LspDocumentSymbols => "lsp.symbols.document",
            Action::LspWorkspaceSymbols => "lsp.symbols.workspace",
            Action::LspRename => "lsp.rename",
            Action::LspCodeAction => "lsp.code_action",
            Action::LspFormat => "lsp.format",
            Action::LspRestart => "lsp.restart",
            Action::EditorApplyTransaction => "editor.apply_transaction",
            Action::AgentSessionNew => "agent.session.new",
            Action::AgentPrompt => "agent.prompt",
            Action::AgentMode => "agent.mode",
            Action::AgentProposalAccept => "agent.proposal.accept",
            Action::AgentProposalReject => "agent.proposal.reject",
            Action::HelpShow => "help.show",
            Action::ConfigReload => "config.reload",
        }
    }

    /// Human-friendly label shown in menus and the command palette.
    pub fn title(&self) -> &'static str {
        match self {
            Action::FileOpen => "Open File",
            Action::FileSave => "Save File",
            Action::FileNew => "New File",
            Action::FolderNew => "New Folder",
            Action::FileRename => "Rename",
            Action::FileDelete => "Delete",
            Action::WorkspaceSearch => "Search in Workspace",
            Action::WorkspaceRestoreDrafts => "Workspace: Restore Drafts",
            Action::PaneSplitRight => "Split Pane Right",
            Action::FocusProject => "Focus Project",
            Action::FocusEditor => "Focus Editor",
            Action::FocusAgent => "Focus Agent",
            Action::TerminalNew => "New Terminal",
            Action::TerminalRun => "Run in Terminal",
            Action::TerminalFocus => "Focus Terminal",
            Action::TerminalNext => "Next Terminal",
            Action::TerminalPrevious => "Previous Terminal",
            Action::TerminalRestart => "Restart Terminal",
            Action::TerminalClose => "Close Terminal",
            Action::TerminalCopyMode => "Terminal Copy Mode",
            // One visible family: the palette is a flat list, so the shared `Git: ` prefix
            // is what makes these eight read as a group (ADR-0010 §5).
            Action::GitShow => "Git: Show Changes",
            Action::GitStage => "Git: Stage File",
            Action::GitUnstage => "Git: Unstage File",
            Action::GitCommit => "Git: Commit",
            Action::GitBranchCheckout => "Git: Switch Branch",
            Action::GitFetch => "Git: Fetch",
            Action::GitPull => "Git: Pull",
            Action::GitPush => "Git: Push",
            Action::TaskRun => "Run Task",
            Action::TaskCancel => "Cancel Task",
            Action::ProblemsShow => "Show Problems",
            Action::ProblemsNext => "Next Problem",
            Action::ProblemsPrevious => "Previous Problem",
            Action::EditorGotoDefinition => "Code: Go to Definition",
            Action::LspHover => "Code: Hover",
            Action::LspCompletion => "Code: Complete",
            Action::LspReferences => "Code: Find References",
            Action::LspDocumentSymbols => "Code: Document Symbols",
            Action::LspWorkspaceSymbols => "Code: Workspace Symbols",
            Action::LspRename => "Code: Rename Symbol",
            Action::LspCodeAction => "Code: Quick Fix",
            Action::LspFormat => "Code: Format Document",
            Action::LspRestart => "Code: Restart Language Server",
            Action::EditorApplyTransaction => "Apply Edit",
            Action::AgentSessionNew => "New Agent Session",
            Action::AgentPrompt => "Prompt Agent",
            Action::AgentMode => "Agent: Session Mode",
            Action::AgentProposalAccept => "Accept Agent Edit",
            Action::AgentProposalReject => "Reject Agent Edit",
            Action::HelpShow => "Help: Keys and Actions",
            Action::ConfigReload => "Config: Reload",
        }
    }

    /// Whether an agent must ask permission before this runs (write/run/commit). §9.4
    pub fn agent_needs_permission(&self) -> bool {
        matches!(
            self,
            Action::FileSave
                | Action::FileNew
                | Action::FolderNew
                | Action::FileRename
                | Action::FileDelete
                | Action::WorkspaceRestoreDrafts
                | Action::TerminalRun
                // Changing the mode changes what the agent is allowed to do, which is
                // exactly the sort of thing it should not be able to do for itself.
                | Action::AgentMode
                | Action::GitStage
                | Action::GitUnstage
                | Action::GitCommit
                | Action::GitBranchCheckout
                | Action::GitFetch
                | Action::GitPull
                | Action::GitPush
                | Action::TaskRun
                | Action::LspRename
                | Action::LspCodeAction
                | Action::LspFormat
                | Action::LspRestart
                | Action::EditorApplyTransaction
        )
    }
}

/// Everything the shell can be told to do. The keymap maps a [`input::KeyChord`] to one
/// of these, and the palette dispatches [`Command::Action`] — one dispatch path for
/// keyboard and palette alike (ARCHITECTURE.md §3, §7.1).
///
/// The agent does *not* dispatch these. ADR-0009 found that stable ACP has no portable
/// way for a client to register its own tools, so the agent's reach into the workspace
/// is the context it is given and the permission-gated operations ACP itself defines —
/// not this enum.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Invoke a registry action (feature-level; shown in the palette).
    Action(Action),
    FocusNext,
    FocusPrev,
    GrowSidebar,
    ShrinkSidebar,
    GrowBottom,
    ShrinkBottom,
    OpenPalette,
    CloseOverlay,
    Quit,
    /// Move the file-explorer selection down one visible row.
    ExplorerNext,
    /// Move the file-explorer selection up one visible row.
    ExplorerPrev,
    /// Expand or collapse the selected directory.
    ExplorerToggle,
    /// Collapse the selection, or step to its parent when already collapsed.
    ExplorerCollapseOrParent,

    // --- editor (Phase 03) --------------------------------------------------------
    //
    // Cursor motion and the named text operations are commands so they stay
    // remappable and reachable from one dispatch path. Only literal character entry
    // is not a command — there is no finite set of "type an x" actions.
    EditorCursorLeft,
    EditorCursorRight,
    EditorCursorUp,
    EditorCursorDown,
    EditorLineStart,
    EditorLineEnd,
    EditorInsertNewline,
    /// Delete backwards from the cursor, or the selection if there is one.
    EditorBackspace,
    /// Delete forwards from the cursor.
    EditorDeleteForward,
    EditorUndo,
    EditorRedo,
    EditorFind,
    EditorFindNext,
    EditorFindPrev,
    EditorReplace,
    EditorNextTab,
    EditorPrevTab,
    EditorCloseTab,

    // --- terminal copy mode (Phase 04) ------------------------------------------
    //
    // Literal terminal input remains data rather than a finite command vocabulary.
    // These commands exist only for model-owned scrollback selection while copy mode
    // is active (ADR-0008 §3).
    TerminalCopyLeft,
    TerminalCopyRight,
    TerminalCopyUp,
    TerminalCopyDown,
    TerminalCopyExtendLeft,
    TerminalCopyExtendRight,
    TerminalCopyExtendUp,
    TerminalCopyExtendDown,
    TerminalCopyPageUp,
    TerminalCopyPageDown,
    TerminalCopyConfirm,
    TerminalCopyCancel,
    /// Move the terminal viewport back through scrollback, without entering copy mode.
    ///
    /// A command rather than an action, for the same reason the agent's scroll is: it
    /// moves a viewport, it is not a standing capability. Copy mode selects text and
    /// takes over the keyboard to do it; reading what just scrolled past should not
    /// require either.
    TerminalScrollUp,
    TerminalScrollDown,

    // --- agent review (Phase 03) --------------------------------------------------
    //
    // Accept/reject are registry *actions* (they are user-invocable features, so the
    // palette and the agent's own tool surface reach them). Permission answers are
    // commands: they are a response to a specific prompt, not a standing capability.
    /// Scroll the agent transcript back through older turns.
    AgentScrollUp,
    AgentScrollDown,
    AgentAllowOnce,
    AgentAllowAlways,
    AgentDeny,
}

impl Command {
    /// Whether this moves focus to one named pane.
    ///
    /// These are the only commands a focused terminal still honours. It swallows every
    /// other chord so the shell gets a real keyboard (ADR-0008 §3) — which is also why
    /// the Terminal cannot sit in the `FocusNext` ring: it would capture the very Tab
    /// that is supposed to carry you out of it, making the pane a one-way door.
    pub fn is_pane_focus(&self) -> bool {
        matches!(
            self,
            Command::Action(
                Action::FocusProject
                    | Action::FocusEditor
                    | Action::FocusAgent
                    | Action::TerminalFocus
            )
        )
    }
}

/// The one command surface: every user-invocable behaviour is an [`Action`] here, and
/// both the keymap and the palette are built from this list.
///
/// It stayed a plain list of variants rather than growing handlers or context
/// predicates. Dispatch lives in the application model, binding lives in
/// `termesh-config`, and neither needs the registry to own a callback to find its way
/// here. Dynamic registration was ruled out rather than deferred: ADR-0009 found that
/// stable ACP has no portable client-owned custom-tool registration, so there is no
/// caller for it.
#[derive(Debug, Default)]
pub struct ActionRegistry {
    actions: Vec<Action>,
}

impl ActionRegistry {
    pub fn with_defaults() -> Self {
        use Action::*;
        Self {
            actions: vec![
                FileOpen,
                FileSave,
                FileNew,
                FolderNew,
                FileRename,
                FileDelete,
                WorkspaceSearch,
                WorkspaceRestoreDrafts,
                PaneSplitRight,
                FocusProject,
                FocusEditor,
                FocusAgent,
                TerminalNew,
                TerminalRun,
                TerminalFocus,
                TerminalNext,
                TerminalPrevious,
                TerminalRestart,
                TerminalClose,
                TerminalCopyMode,
                GitShow,
                GitStage,
                GitUnstage,
                GitCommit,
                GitBranchCheckout,
                GitFetch,
                GitPull,
                GitPush,
                TaskRun,
                TaskCancel,
                ProblemsShow,
                ProblemsNext,
                ProblemsPrevious,
                EditorGotoDefinition,
                LspHover,
                LspCompletion,
                LspReferences,
                LspDocumentSymbols,
                LspWorkspaceSymbols,
                LspRename,
                LspCodeAction,
                LspFormat,
                LspRestart,
                EditorApplyTransaction,
                AgentSessionNew,
                AgentPrompt,
                AgentMode,
                AgentProposalAccept,
                AgentProposalReject,
                HelpShow,
                ConfigReload,
            ],
        }
    }
    pub fn len(&self) -> usize {
        self.actions.len()
    }
    pub fn is_empty(&self) -> bool {
        self.actions.is_empty()
    }
    pub fn ids(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.actions.iter().map(Action::id)
    }
    pub fn actions(&self) -> &[Action] {
        &self.actions
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_exposes_stable_action_ids() {
        let reg = ActionRegistry::with_defaults();
        assert_eq!(reg.len(), 51);
        assert!(reg.ids().any(|id| id == "agent.prompt"));
        assert!(reg.ids().any(|id| id == "focus.project"));
        assert!(reg.ids().all(|id| id.contains('.')));
    }

    #[test]
    fn phase_10_help_action_is_stable_and_read_only() {
        assert_eq!(Action::HelpShow.id(), "help.show");
        assert_eq!(Action::HelpShow.title(), "Help: Keys and Actions");
        assert!(!Action::HelpShow.agent_needs_permission());
        assert!(ActionRegistry::with_defaults().actions().contains(&Action::HelpShow));
    }

    #[test]
    fn phase_10_draft_restore_action_is_stable_and_permission_gated() {
        assert_eq!(Action::WorkspaceRestoreDrafts.id(), "workspace.restore_drafts");
        assert_eq!(Action::WorkspaceRestoreDrafts.title(), "Workspace: Restore Drafts");
        assert!(Action::WorkspaceRestoreDrafts.agent_needs_permission());
        assert!(ActionRegistry::with_defaults()
            .actions()
            .contains(&Action::WorkspaceRestoreDrafts));
    }

    #[test]
    fn phase_07_actions_have_stable_ids_and_permissions() {
        assert_eq!(Action::EditorGotoDefinition.id(), "editor.goto_definition");
        assert_eq!(Action::LspHover.id(), "lsp.hover");
        assert_eq!(Action::LspCompletion.id(), "lsp.completion");
        assert_eq!(Action::LspReferences.id(), "lsp.references");
        assert_eq!(Action::LspDocumentSymbols.id(), "lsp.symbols.document");
        assert_eq!(Action::LspWorkspaceSymbols.id(), "lsp.symbols.workspace");
        assert_eq!(Action::LspRename.id(), "lsp.rename");
        assert_eq!(Action::LspCodeAction.id(), "lsp.code_action");
        assert_eq!(Action::LspFormat.id(), "lsp.format");
        assert_eq!(Action::LspRestart.id(), "lsp.restart");

        // One visible family in the flat palette.
        for action in [
            Action::EditorGotoDefinition,
            Action::LspHover,
            Action::LspCompletion,
            Action::LspReferences,
            Action::LspDocumentSymbols,
            Action::LspWorkspaceSymbols,
            Action::LspRename,
            Action::LspCodeAction,
            Action::LspFormat,
            Action::LspRestart,
        ] {
            assert!(action.title().starts_with("Code: "), "{}", action.id());
        }

        // Reads are free; anything that edits a buffer or starts a process is gated.
        for action in [
            Action::EditorGotoDefinition,
            Action::LspHover,
            Action::LspCompletion,
            Action::LspReferences,
            Action::LspDocumentSymbols,
            Action::LspWorkspaceSymbols,
        ] {
            assert!(!action.agent_needs_permission(), "{}", action.id());
        }
        for action in
            [Action::LspRename, Action::LspCodeAction, Action::LspFormat, Action::LspRestart]
        {
            assert!(action.agent_needs_permission(), "{}", action.id());
        }
    }

    #[test]
    fn every_action_has_a_title() {
        for a in ActionRegistry::with_defaults().actions() {
            assert!(!a.title().is_empty());
        }
    }

    #[test]
    fn write_actions_are_permission_gated_for_agents() {
        assert!(Action::GitCommit.agent_needs_permission());
        assert!(!Action::EditorGotoDefinition.agent_needs_permission());
    }

    #[test]
    fn terminal_actions_have_stable_ids() {
        assert_eq!(Action::TerminalFocus.id(), "terminal.focus");
        assert_eq!(Action::TerminalNext.id(), "terminal.next");
        assert_eq!(Action::TerminalPrevious.id(), "terminal.previous");
        assert_eq!(Action::TerminalRestart.id(), "terminal.restart");
        assert_eq!(Action::TerminalClose.id(), "terminal.close");
        assert_eq!(Action::TerminalCopyMode.id(), "terminal.copy_mode");
    }

    #[test]
    fn agent_terminal_run_is_permission_gated() {
        assert!(Action::TerminalRun.agent_needs_permission());
    }

    #[test]
    fn phase_05_actions_have_stable_ids_and_permissions() {
        assert_eq!(Action::TaskCancel.id(), "task.cancel");
        assert_eq!(Action::ProblemsShow.id(), "problems.show");
        assert_eq!(Action::ProblemsNext.id(), "problems.next");
        assert_eq!(Action::ProblemsPrevious.id(), "problems.previous");
        assert!(Action::TaskRun.agent_needs_permission());
        assert!(!Action::TaskCancel.agent_needs_permission());
    }

    #[test]
    fn phase_06_actions_have_stable_ids_and_permissions() {
        assert_eq!(Action::GitShow.id(), "git.show");
        assert_eq!(Action::GitStage.id(), "git.stage");
        assert_eq!(Action::GitUnstage.id(), "git.unstage");
        assert_eq!(Action::GitCommit.id(), "git.commit");
        assert_eq!(Action::GitBranchCheckout.id(), "git.branch.checkout");
        assert_eq!(Action::GitFetch.id(), "git.fetch");
        assert_eq!(Action::GitPull.id(), "git.pull");
        assert_eq!(Action::GitPush.id(), "git.push");
        assert!(!Action::GitShow.agent_needs_permission());
        // The palette is one flat list; a shared prefix is the only grouping it has.
        for action in [
            Action::GitShow,
            Action::GitStage,
            Action::GitUnstage,
            Action::GitCommit,
            Action::GitBranchCheckout,
            Action::GitFetch,
            Action::GitPull,
            Action::GitPush,
        ] {
            assert!(action.title().starts_with("Git: "), "{}", action.title());
        }
        for action in [
            Action::GitStage,
            Action::GitUnstage,
            Action::GitCommit,
            Action::GitBranchCheckout,
            Action::GitFetch,
            Action::GitPull,
            Action::GitPush,
        ] {
            assert!(action.agent_needs_permission(), "{}", action.id());
        }
    }
}
