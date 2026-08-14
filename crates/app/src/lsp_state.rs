//! Single-owner language-intelligence state (ADR-0011).

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use ropey::Rope;
use termesh_core::{
    BufferId, CodeAction, CompletionItem, Diagnostic, HoverText, Location, LspFailure,
    LspRequestId, LspServerId, WorkspaceEdit,
};
use termesh_ui::Pane;

#[derive(Debug, Clone)]
pub struct HoverOverlay {
    pub hover: HoverText,
    pub previous_focus: Pane,
}

#[derive(Debug, Clone)]
pub struct CompletionOverlay {
    pub items: Vec<CompletionItem>,
    pub selected: usize,
    pub previous_focus: Pane,
}

impl CompletionOverlay {
    pub fn move_down(&mut self) {
        if !self.items.is_empty() {
            self.selected = (self.selected + 1) % self.items.len();
        }
    }

    pub fn move_up(&mut self) {
        if !self.items.is_empty() {
            self.selected = (self.selected + self.items.len() - 1) % self.items.len();
        }
    }
}

#[derive(Debug, Clone)]
pub struct CodeActionOverlay {
    pub actions: Vec<CodeAction>,
    pub selected: usize,
    pub previous_focus: Pane,
}

impl CodeActionOverlay {
    pub fn move_down(&mut self) {
        if !self.actions.is_empty() {
            self.selected = (self.selected + 1) % self.actions.len();
        }
    }

    pub fn move_up(&mut self) {
        if !self.actions.is_empty() {
            self.selected = (self.selected + self.actions.len() - 1) % self.actions.len();
        }
    }
}

/// A server edit waiting for every closed target to arrive through the filesystem
/// worker. No transaction is applied until `waiting` is empty.
#[derive(Debug)]
pub struct PendingWorkspaceEdit {
    pub edit: WorkspaceEdit,
    pub waiting: BTreeMap<BufferId, PathBuf>,
    pub previous_active: Option<BufferId>,
}

#[derive(Debug, Clone)]
pub struct ReferencesOverlay {
    pub locations: Vec<Location>,
    pub selected: usize,
    pub previous_focus: Pane,
}

impl ReferencesOverlay {
    pub fn move_down(&mut self) {
        if !self.locations.is_empty() {
            self.selected = (self.selected + 1) % self.locations.len();
        }
    }

    pub fn move_up(&mut self) {
        if !self.locations.is_empty() {
            self.selected = (self.selected + self.locations.len() - 1) % self.locations.len();
        }
    }
}

#[derive(Debug, Clone)]
pub struct SymbolRow {
    pub label: String,
    pub detail: Option<String>,
    pub depth: usize,
    pub location: Location,
}

#[derive(Debug, Clone)]
pub struct SymbolsOverlay {
    pub title: String,
    pub rows: Vec<SymbolRow>,
    pub selected: usize,
    pub previous_focus: Pane,
}

impl SymbolsOverlay {
    pub fn move_down(&mut self) {
        if !self.rows.is_empty() {
            self.selected = (self.selected + 1) % self.rows.len();
        }
    }

    pub fn move_up(&mut self) {
        if !self.rows.is_empty() {
            self.selected = (self.selected + self.rows.len() - 1) % self.rows.len();
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LspLoadState {
    /// A configured recipe that has not been asked to start yet (ADR-0012 §2).
    Idle,
    Starting,
    Indexing {
        message: String,
        percent: Option<u8>,
    },
    Ready,
    Unavailable(LspFailure),
    Stale(LspFailure),
}

/// Everything needed to launch this session's process again.
///
/// Kept on the session so `lsp.restart` can relaunch the server it already resolved,
/// without re-reading workspace configuration — the model reaches the filesystem only
/// through the worker, so a synchronous re-read is not available to it.
#[derive(Debug, Clone)]
pub struct SessionLaunch {
    pub root: PathBuf,
    pub command: Vec<String>,
    pub initialization_options: Option<String>,
}

/// A recipe resolved at workspace open but not launched until a claimed document opens.
#[derive(Debug, Clone)]
pub struct ConfiguredRecipe {
    pub language: String,
    pub extensions: Vec<String>,
    pub launch: SessionLaunch,
    pub load: LspLoadState,
}

impl ConfiguredRecipe {
    pub fn new(language: String, extensions: Vec<String>, launch: SessionLaunch) -> Self {
        Self { language, extensions, launch, load: LspLoadState::Idle }
    }

    fn claims(&self, path: &Path) -> bool {
        let Some(extension) = path.extension().and_then(|extension| extension.to_str()) else {
            return false;
        };
        self.extensions.iter().any(|claimed| claimed.eq_ignore_ascii_case(extension))
    }
}

/// One language server and everything currently in flight against it.
#[derive(Debug)]
pub struct LspSessionState {
    pub server: LspServerId,
    pub language: String,
    pub load: LspLoadState,
    pub extensions: Vec<String>,
    pub launch: SessionLaunch,
    /// Protocol versions are independent from editor undo/redo versions.
    pub open_docs: HashMap<PathBuf, u64>,
    pub next_doc_version: u64,
    /// The pre-image used to derive exact incremental ranges and replacement text.
    pub(crate) synced_docs: HashMap<PathBuf, Rope>,
    pub active_definition: Option<LspRequestId>,
    pub active_hover: Option<LspRequestId>,
    pub active_completion: Option<LspRequestId>,
    pub active_references: Option<LspRequestId>,
    pub active_document_symbols: Option<LspRequestId>,
    pub active_document_symbol_path: Option<PathBuf>,
    pub active_workspace_symbols: Option<LspRequestId>,
    pub active_rename: Option<LspRequestId>,
    pub active_code_actions: Option<LspRequestId>,
    pub active_formatting: Option<LspRequestId>,
}

impl LspSessionState {
    pub fn new(
        server: LspServerId,
        language: String,
        extensions: Vec<String>,
        launch: SessionLaunch,
    ) -> Self {
        Self {
            server,
            language,
            load: LspLoadState::Starting,
            extensions,
            launch,
            open_docs: HashMap::new(),
            next_doc_version: 0,
            synced_docs: HashMap::new(),
            active_definition: None,
            active_hover: None,
            active_completion: None,
            active_references: None,
            active_document_symbols: None,
            active_document_symbol_path: None,
            active_workspace_symbols: None,
            active_rename: None,
            active_code_actions: None,
            active_formatting: None,
        }
    }

    pub fn next_document_version(&mut self) -> u64 {
        self.next_doc_version += 1;
        self.next_doc_version
    }

    /// Forget every fact that belonged to the previous process.
    ///
    /// Wire versions restart at zero because a relaunched server has never seen these
    /// documents, and clearing `open_docs` is what makes `sync_lsp_documents` reopen
    /// them: the reset session no longer claims to have them (ADR-0011 §4).
    pub fn reset_for_restart(&mut self) {
        self.load = LspLoadState::Starting;
        self.open_docs.clear();
        self.synced_docs.clear();
        self.next_doc_version = 0;
        self.active_document_symbol_path = None;
        for active in [
            &mut self.active_definition,
            &mut self.active_hover,
            &mut self.active_completion,
            &mut self.active_references,
            &mut self.active_document_symbols,
            &mut self.active_workspace_symbols,
            &mut self.active_rename,
            &mut self.active_code_actions,
            &mut self.active_formatting,
        ] {
            *active = None;
        }
    }

    pub fn clear_request(&mut self, id: LspRequestId) -> bool {
        if self.active_document_symbols == Some(id) {
            self.active_document_symbol_path = None;
        }
        let mut matched = false;
        for active in [
            &mut self.active_definition,
            &mut self.active_hover,
            &mut self.active_completion,
            &mut self.active_references,
            &mut self.active_document_symbols,
            &mut self.active_workspace_symbols,
            &mut self.active_rename,
            &mut self.active_code_actions,
            &mut self.active_formatting,
        ] {
            if *active == Some(id) {
                *active = None;
                matched = true;
            }
        }
        matched
    }
}

#[derive(Debug, Default)]
pub struct LspState {
    /// Recipes resolved at workspace open and not yet launched (ADR-0012 §2).
    pub configured: Vec<ConfiguredRecipe>,
    /// Live sessions keyed by stable application-owned server identity (ADR-0011 §1).
    pub sessions: BTreeMap<LspServerId, LspSessionState>,
    /// A document belongs to exactly one server, so diagnostics can be keyed by path.
    pub diagnostics: BTreeMap<PathBuf, Vec<Diagnostic>>,
}

impl LspState {
    /// Return the session that claims `path`, or refuse to guess.
    pub fn server_for(&self, path: &Path) -> Option<LspServerId> {
        let extension = path.extension()?.to_str()?;
        self.sessions.iter().find_map(|(_, session)| {
            session
                .extensions
                .iter()
                .any(|claimed| claimed.eq_ignore_ascii_case(extension))
                .then_some(session.server)
        })
    }

    /// Return the configured recipe index that claims `path`, or refuse to guess.
    pub fn recipe_for_path(&self, path: &Path) -> Option<usize> {
        self.configured
            .iter()
            .position(|recipe| matches!(recipe.load, LspLoadState::Idle) && recipe.claims(path))
    }
}
