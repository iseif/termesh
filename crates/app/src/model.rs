//! Single-owner application state and the one dispatch path (ARCHITECTURE.md §7.1).
//! Every command — from a keybinding or the palette — flows through [`Model::dispatch`].
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};
use termesh_agent::EditProposal;
use termesh_config::{default_keymap, Autosave, ConfigDiagnostic, Keymap, Settings, ThemeChoice};
use termesh_core::input::KeyContext;
use termesh_core::{
    Action, ActionRegistry, AgentCapabilities, AgentEvent, AgentRequest, AgentTerminalOperation,
    AgentTerminalRequestId, AgentTerminalResponse, BufferId, CodeAction, Command, CompletionItem,
    DocumentSymbol, FsEvent, FsRequest, GitEvent, GitFailureKind, GitRequest, GitRequestId,
    LocationRequestId, LspEvent, LspFailure, LspFailureKind, LspRequest, LspRequestId, LspServerId,
    PermissionRequestId, PreviewRequestId, ProposalId, PtyEvent, PtyRequest, ReadRequestId,
    SearchEvent, SearchMode, SearchRequest, SearchRequestId, SessionId, StopReason,
    TerminalGeneration, TerminalId, TerminalOwner, TerminalSize, TerminalSpec, TerminalStatus,
    TextChange, TextEdit, TextPosition, TextRange, WatchedFileChange, WorkspaceEdit,
};
use termesh_editor::{Buffer, ChangeSet, Decoration, DecorationClass, EditSource, HunkSide};

use termesh_filesystem::{DirReader, FileSystemService, FileTree, IgnoreOptions};
use termesh_terminal::{CapturedOutput, TerminalScreen, DEFAULT_CAPTURE_LIMIT};
use termesh_ui::{LayoutState, Pane, Theme};
use termesh_workspace::{
    AgentHistoryLine, AgentHistorySpeaker, LanguageSettings, PermissionPolicy, RestoredWorkspace,
    Session, WorkspaceRoot, WorkspaceSnapshot,
};

use crate::git_state::{
    GitBranchesOverlay, GitDiffOverlay, GitLoadState, GitState, GitStatusOverlay,
};
use crate::lsp_state::{
    CodeActionOverlay, CompletionOverlay, ConfiguredRecipe, HoverOverlay, LspLoadState,
    LspSessionState, LspState, PendingWorkspaceEdit, ReferencesOverlay, SessionLaunch, SymbolRow,
    SymbolsOverlay,
};
use crate::search_state::{SearchOverlay, SearchStatus};
use crate::task_state::{ProblemRow, ProblemsOverlay, TaskPicker, TaskRun};

/// The command palette overlay. Snapshots the action list on open so filtering never
/// needs to borrow the registry (keeps the borrow checker and the render path simple).
pub struct Palette {
    pub query: String,
    items: Vec<(Action, String, String)>, // (action, label, key hint)
    filtered: Vec<usize>,
    pub selected: usize,
}

impl Palette {
    pub fn open(registry: &ActionRegistry, keymap: &Keymap) -> Self {
        let items = registry
            .actions()
            .iter()
            .map(|a| {
                let hint = keymap
                    .chord_for(&Command::Action(a.clone()))
                    .map(|c| c.to_string())
                    .unwrap_or_default();
                (a.clone(), a.title().to_string(), hint)
            })
            .collect();
        let mut p = Self { query: String::new(), items, filtered: Vec::new(), selected: 0 };
        p.refilter();
        p
    }

    pub fn total(&self) -> usize {
        self.items.len()
    }

    fn refilter(&mut self) {
        let q = self.query.to_lowercase();
        self.filtered = self
            .items
            .iter()
            .enumerate()
            .filter(|(_, (a, label, _))| {
                q.is_empty() || subsequence(&q, &label.to_lowercase()) || a.id().contains(&q)
            })
            .map(|(i, _)| i)
            .collect();
        if self.selected >= self.filtered.len() {
            self.selected = self.filtered.len().saturating_sub(1);
        }
    }

    /// `(label, hint)` pairs for the currently filtered view.
    pub fn view_items(&self) -> Vec<(String, String)> {
        self.filtered.iter().map(|&i| (self.items[i].1.clone(), self.items[i].2.clone())).collect()
    }

    pub fn selected_action(&self) -> Option<Action> {
        self.filtered.get(self.selected).map(|&i| self.items[i].0.clone())
    }

    pub fn push_char(&mut self, c: char) {
        self.query.push(c);
        self.refilter();
    }
    pub fn pop_char(&mut self) {
        self.query.pop();
        self.refilter();
    }
    pub fn move_down(&mut self) {
        if !self.filtered.is_empty() {
            self.selected = (self.selected + 1) % self.filtered.len();
        }
    }
    pub fn move_up(&mut self) {
        if !self.filtered.is_empty() {
            self.selected = (self.selected + self.filtered.len() - 1) % self.filtered.len();
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HelpRow {
    pub group: &'static str,
    pub id: &'static str,
    pub title: &'static str,
    /// Canonical keymap spelling, so a row can be compared with `keymap.toml` directly.
    pub chord: Option<String>,
}

pub struct HelpOverlay {
    pub query: String,
    rows: Vec<HelpRow>,
    filtered: Vec<usize>,
    pub scroll: usize,
    pub previous_focus: Pane,
}

impl HelpOverlay {
    fn open(registry: &ActionRegistry, keymap: &Keymap, previous_focus: Pane) -> Self {
        let rows = registry
            .actions()
            .iter()
            .map(|action| HelpRow {
                group: help_group(action.id()),
                id: action.id(),
                title: action.title(),
                chord: keymap
                    .chord_for(&Command::Action(action.clone()))
                    .map(|chord| chord.to_string().to_ascii_lowercase()),
            })
            .collect();
        let mut help =
            Self { query: String::new(), rows, filtered: Vec::new(), scroll: 0, previous_focus };
        help.refilter();
        help
    }

    fn refilter(&mut self) {
        let query = self.query.to_ascii_lowercase();
        self.filtered = self
            .rows
            .iter()
            .enumerate()
            .filter(|(_, row)| {
                query.is_empty()
                    || subsequence(&query, &row.title.to_ascii_lowercase())
                    || row.id.contains(&query)
                    || row.group.to_ascii_lowercase().contains(&query)
                    || row.chord.as_ref().is_some_and(|chord| chord.contains(&query))
            })
            .map(|(index, _)| index)
            .collect();
        self.scroll = self.scroll.min(self.filtered.len().saturating_sub(1));
    }

    pub fn visible_rows(&self) -> Vec<HelpRow> {
        self.filtered.iter().map(|index| self.rows[*index].clone()).collect()
    }

    #[cfg(test)]
    pub fn all_rows(&self) -> Vec<HelpRow> {
        self.rows.clone()
    }

    pub fn push_char(&mut self, character: char) {
        self.query.push(character);
        self.scroll = 0;
        self.refilter();
    }

    pub fn pop_char(&mut self) {
        self.query.pop();
        self.scroll = 0;
        self.refilter();
    }

    pub fn scroll_by(&mut self, amount: isize) {
        self.scroll =
            self.scroll.saturating_add_signed(amount).min(self.filtered.len().saturating_sub(1));
    }
}

fn help_group(id: &str) -> &'static str {
    match id.split('.').next().unwrap_or_default() {
        "file" => "File",
        "workspace" => "Workspace",
        "pane" | "focus" => "Window",
        "terminal" => "Terminal",
        "git" => "Git",
        "task" => "Tasks",
        "problems" => "Problems",
        "editor" | "lsp" => "Code",
        "agent" => "Agent",
        "help" => "Help",
        "config" => "Config",
        _ => "Other",
    }
}

/// What a [`Prompt`] will do when confirmed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptKind {
    /// Create a file inside `parent`.
    NewFile {
        parent: PathBuf,
    },
    /// Create a directory inside `parent`.
    NewDir {
        parent: PathBuf,
    },
    Rename {
        target: PathBuf,
    },
    /// Destructive, so it is a confirmation rather than a text entry: the input is
    /// ignored and Enter means "yes". Nothing is deleted without passing through here.
    ConfirmDelete {
        target: PathBuf,
        is_dir: bool,
    },
    /// A turn to send to the agent.
    AgentPrompt {
        session: SessionId,
    },
    /// A literal string to find in the active buffer.
    Find,
    /// What to replace every occurrence of the current query with.
    Replace,
    /// Close a buffer with unsaved changes. A confirmation, not a text entry — the same
    /// shape as a delete, for the same reason: the work is gone either way.
    ConfirmCloseBuffer {
        buffer: BufferId,
    },
    /// A human-authored command line to pass to the platform shell.
    TerminalRun,
    /// Commit exactly the index with the supplied message.
    GitCommit,
    /// Rename the symbol at the active editor cursor through its language server.
    LspRename,
    /// Closing a live process is destructive and therefore explicit.
    ConfirmCloseTerminal {
        terminal: TerminalId,
    },
}

/// A single-line input (or confirmation) overlay.
pub struct Prompt {
    pub title: String,
    pub input: String,
    pub kind: PromptKind,
}

impl Prompt {
    /// Whether this prompt takes typed input, as opposed to being a yes/no confirmation.
    pub fn takes_input(&self) -> bool {
        !matches!(
            self.kind,
            PromptKind::ConfirmDelete { .. }
                | PromptKind::ConfirmCloseBuffer { .. }
                | PromptKind::ConfirmCloseTerminal { .. }
        )
    }
}

pub enum Overlay {
    Palette(Palette),
    Help(HelpOverlay),
    Prompt(Prompt),
    Search(SearchOverlay),
    Tasks(TaskPicker),
    AgentModes(AgentModePicker),
    Problems(ProblemsOverlay),
    GitStatus(GitStatusOverlay),
    GitDiff(GitDiffOverlay),
    GitBranches(GitBranchesOverlay),
    Hover(HoverOverlay),
    Completion(CompletionOverlay),
    CodeActions(CodeActionOverlay),
    References(ReferencesOverlay),
    Symbols(SymbolsOverlay),
    DraftRecovery(DraftRecoveryOverlay),
}

pub struct DraftRecoveryOverlay {
    pub drafts: Vec<termesh_workspace::drafts::Draft>,
    pub selected: usize,
    pub chosen: Vec<bool>,
    pub previous_focus: Pane,
}

/// The opened project: its detected root plus the lazy tree over it.
pub struct Explorer {
    pub root: WorkspaceRoot,
    pub tree: FileTree,
}

/// Who said something in the transcript.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Speaker {
    You,
    Agent,
    /// The agent's reasoning, rendered apart from its answer.
    Thought,
}

/// One turn of the conversation.
///
/// A *turn*, not a chunk. ACP streams `agent_message_chunk` many times per sentence, so
/// keeping one entry per chunk shatters a flowing paragraph into fragments — consecutive
/// chunks from the same speaker are appended to the same entry instead.
#[derive(Debug, Clone)]
pub struct TranscriptLine {
    pub speaker: Speaker,
    pub text: String,
}

/// A live agent conversation and everything awaiting the human's judgement.
pub struct AgentSession {
    pub id: SessionId,
    /// The conversation, oldest first. Bounded so a long turn cannot grow without limit
    /// (ARCHITECTURE.md §19 — agent-context caches stay bounded).
    pub transcript: Vec<TranscriptLine>,
    pub proposals: Vec<EditProposal>,
    /// A tool call awaiting approval. One at a time: the agent blocks on the answer.
    pub pending_permission: Option<PendingPermission>,
    /// Managed terminals referenced by this conversation, retained after ACP release.
    pub attached_terminals: Vec<TerminalId>,
    pub turn_active: bool,
    /// What this agent will let the session do, and which of those it is in (ADR-0015).
    /// Empty for the agents that offer no choice, which is most of them.
    pub modes: Vec<termesh_core::SessionMode>,
    pub current_mode: Option<String>,
}

/// The agent's own list of what it will let this session do (ADR-0015 §3).
///
/// Holds the modes as the agent described them rather than a rendered list, so the picker
/// can show each one's description — which is the only place the meaning of a name like
/// `full-access` is written down.
pub struct AgentModePicker {
    pub modes: Vec<termesh_core::SessionMode>,
    pub current: Option<String>,
    pub selected: usize,
}

impl AgentModePicker {
    pub fn move_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    pub fn move_down(&mut self) {
        if self.selected + 1 < self.modes.len() {
            self.selected += 1;
        }
    }

    pub fn selected(&self) -> Option<&termesh_core::SessionMode> {
        self.modes.get(self.selected)
    }
}

/// Which protocol exchange is blocked on the permission prompt.
pub enum PermissionOrigin {
    AgentRequest {
        request: PermissionRequestId,
        terminal_spec: Option<TerminalSpec>,
    },
    TerminalCreate {
        session: SessionId,
        request: AgentTerminalRequestId,
        spec: TerminalSpec,
        output_byte_limit: usize,
    },
}

/// A tool call the agent wants to run, shown before it runs (ARCHITECTURE.md §9.4).
pub struct PendingPermission {
    pub origin: PermissionOrigin,
    pub summary: String,
    /// Argv array. Never a shell string — we do not interpolate agent output into one.
    pub command: Vec<String>,
    /// The proposal this permission is gating, when the agent described an edit we could
    /// place in the buffer. Answering the permission *is* the decision: the agent does the
    /// writing, so this proposal is displayed and then discarded, never applied
    /// (ADR-0016 §2).
    pub review: Option<ProposalId>,
}

struct PendingTerminalCreate {
    request: AgentTerminalRequestId,
    terminal: TerminalId,
    generation: TerminalGeneration,
}

struct PendingTerminalWait {
    request: AgentTerminalRequestId,
    terminal: TerminalId,
}

/// The most recent text we served the agent for a path, and the buffer revision it came
/// from (ADR-0007 §5).
///
/// This is what lets a proposal anchor to a revision we hold: because *we* answer
/// `fs/read_text_file`, we know exactly what the agent read and when.
struct ServedRead {
    path: PathBuf,
    version: termesh_editor::Version,
    text: String,
}

struct PendingProblemNavigation {
    request: LocationRequestId,
    problem: termesh_core::Problem,
}

/// A `fs/read_text_file` answer waiting on a worker read for a file the agent asked
/// about that is not open in any buffer. Never promoted to a visible buffer — the
/// human did not ask to open it, only the agent asked to read it.
struct PendingAgentRead {
    buffer: BufferId,
    session: SessionId,
    request: ReadRequestId,
    path: PathBuf,
}

/// Which config file a [`PendingConfigReload`] is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConfigReloadKind {
    Settings,
    Keymap,
}

/// A `config.reload` read waiting on the filesystem worker. Reading `config.toml` and
/// `keymap.toml` is blocking I/O, so `config.reload` queues it through the worker like
/// every other read rather than calling `FileSystemService` from the render loop
/// (ARCHITECTURE.md's single-owner/no-blocking-I/O invariant) — the same reasoning as
/// [`PendingAgentRead`], just for a file the human, not the agent, asked to reread.
struct PendingConfigReload {
    buffer: BufferId,
    kind: ConfigReloadKind,
    path: PathBuf,
}

/// Buffer reads belonging to one startup restore. Responses may arrive in any order;
/// persisted tab order and active path remain authoritative.
struct PendingSessionRestore {
    order: Vec<PathBuf>,
    active: Option<PathBuf>,
    pending: HashSet<BufferId>,
}

/// A live find in the active buffer.
#[derive(Debug, Default, Clone)]
pub struct Find {
    pub query: String,
    pub matches: Vec<termesh_editor::Match>,
    /// Index into `matches`. `None` when the query found nothing.
    pub current: Option<usize>,
}

/// Model-owned state for one retained terminal tab.
pub struct TerminalSession {
    pub id: TerminalId,
    pub generation: TerminalGeneration,
    pub spec: TerminalSpec,
    pub owner: TerminalOwner,
    pub title: String,
    pub status: TerminalStatus,
    pub screen: TerminalScreen,
    pub capture: CapturedOutput,
    pub released: bool,
}

/// How much transcript to keep.
const TRANSCRIPT_LIMIT: usize = 500;

struct BoundedContext {
    text: String,
    content_limit: usize,
    truncated: bool,
    marker: &'static str,
}

impl BoundedContext {
    fn new(limit: usize, marker: &'static str) -> Self {
        Self {
            text: String::new(),
            content_limit: limit.saturating_sub(marker.len() + 1),
            truncated: false,
            marker,
        }
    }

    fn line(&mut self, value: &str) -> bool {
        let separator = usize::from(!self.text.is_empty() && !self.text.ends_with('\n'));
        let needed = separator + value.len() + 1;
        if self.text.len().saturating_add(needed) > self.content_limit {
            self.truncated = true;
            return false;
        }
        if separator == 1 {
            self.text.push('\n');
        }
        self.text.push_str(value);
        self.text.push('\n');
        true
    }

    fn text(&mut self, value: &str) -> bool {
        let available = self.content_limit.saturating_sub(self.text.len());
        let mut end = available.min(value.len());
        while end > 0 && !value.is_char_boundary(end) {
            end -= 1;
        }
        self.text.push_str(&value[..end]);
        if end < value.len() {
            self.truncated = true;
            false
        } else {
            true
        }
    }

    fn finish(mut self) -> String {
        while self.text.ends_with('\n') {
            self.text.pop();
        }
        if self.truncated {
            if !self.text.is_empty() {
                self.text.push('\n');
            }
            self.text.push_str(self.marker);
        }
        self.text
    }
}

pub struct Model {
    pub running: bool,
    pub focus: Pane,
    pub layout: LayoutState,
    pub overlays: Vec<Overlay>,
    pub registry: ActionRegistry,
    pub keymap: Keymap,
    pub theme: Theme,
    /// `~/.config/<app>/config.toml`, layered over the compiled defaults (ADR-0014 §3).
    /// `soft_wrap` is parsed and stored but not yet applied; its consumer lands in
    /// Phase 11. `autosave` drives crash-recovery drafts in this phase.
    pub settings: Settings,
    pub notification: Option<String>,
    first_run: bool,
    /// `None` until a workspace is opened — running with no project is legitimate.
    pub explorer: Option<Explorer>,
    /// What the explorer hides. Ignored and hidden entries are off by default so the
    /// tree — and the agent's view of it — looks like the project (ADR-0005 §4).
    pub ignore_options: IgnoreOptions,
    /// Open buffers, in the order they were opened — the tab strip's order once tabs land.
    pub buffers: Vec<Buffer>,
    /// Index into [`Self::buffers`]. `None` when nothing is open.
    pub active_buffer: Option<usize>,
    next_buffer_id: u64,
    /// Files whose read is in flight, so a second Enter on the same row does not queue a
    /// duplicate read and end up with the file open twice.
    opening: Vec<(BufferId, PathBuf)>,
    /// The highlighter, kept between edits so the grammar is loaded once rather than per
    /// keystroke. Rebuilt when the active file's language changes.
    highlighter: Option<(termesh_syntax::Language, termesh_syntax::Highlighter)>,
    /// The active find, if one is running.
    pub find: Option<Find>,
    /// The configured agent's name, once one has been started. `None` means Tier 0 —
    /// nothing configured — which the Agent pane must say rather than looking identical
    /// to a connected agent that simply has no session yet.
    pub agent_name: Option<String>,
    /// A turn typed before a session existed, sent as soon as one does.
    pending_prompt: Option<String>,
    /// Wrapped lines scrolled back from the newest in the Agent pane. 0 follows the
    /// conversation; scrolling up pins the view so incoming text does not yank it.
    pub agent_scroll: usize,
    /// How far back the pane can actually go, learned from the last render.
    pub agent_scroll_max: usize,
    /// The live agent conversation, if one has been started.
    pub agent: Option<AgentSession>,
    /// Transcript from the previous process, displayed but never replayed to ACP. Keeping
    /// it separate from `agent` is what makes the read-only boundary structural.
    pub restored_agent_history: Vec<TranscriptLine>,
    /// What the connected agent told us it can do, from the `initialize` handshake.
    /// `None` before the handshake completes. Connection-level, not session-level: it
    /// outlives any one `AgentSession` (ADR-0014 §4). Recorded and reported this phase;
    /// Phase 11 gates behaviour on it.
    pub agent_capabilities: Option<AgentCapabilities>,
    permission_policy: PermissionPolicy,
    pending_terminal_creates: Vec<PendingTerminalCreate>,
    pending_terminal_waits: Vec<PendingTerminalWait>,
    /// Retained terminal tabs, in tab-strip order.
    pub terminals: Vec<TerminalSession>,
    /// Index into [`Self::terminals`].
    active_terminal: Option<usize>,
    next_terminal_id: u64,
    previous_non_terminal_focus: Pane,
    terminal_size: TerminalSize,
    terminal_outbox: Vec<PtyRequest>,
    terminal_copy_mode: bool,
    clipboard_outbox: Vec<String>,
    /// Work for the agent worker, queued the same way filesystem work is.
    agent_outbox: Vec<AgentRequest>,
    /// What we have served the agent, newest last (ADR-0007 §5).
    served_reads: Vec<ServedRead>,
    /// Reads for files not open in a buffer, in flight to the filesystem worker. Keyed
    /// by the `BufferId` the read was issued under so `FsEvent::FileLoaded`/`FileFailed`
    /// can be told apart from an ordinary buffer open (ADR-0014 Task 1).
    pending_agent_reads: Vec<PendingAgentRead>,
    /// `config.toml`/`keymap.toml` reads in flight for `config.reload`, keyed the same
    /// way [`Self::pending_agent_reads`] is (ADR-0014 Task 5).
    pending_config_reloads: Vec<PendingConfigReload>,
    pending_session_restore: Option<PendingSessionRestore>,
    /// OS-private crash-recovery storage. Drafts never touch the user's project files.
    drafts_dir: Option<PathBuf>,
    draft_versions: BTreeMap<PathBuf, u64>,
    draft_deadline: Option<Instant>,
    pending_draft_writes: BTreeMap<BufferId, PathBuf>,
    known_drafts: HashSet<PathBuf>,
    /// Text rows in the editor pane, so cursor commands can scroll.
    ///
    /// Set by whoever is about to draw; the default is a plausible terminal so the model
    /// behaves sensibly before the first frame. `render` never writes it — it stays a
    /// pure function of the model (ARCHITECTURE.md §7.1).
    editor_height: usize,
    search_outbox: Vec<SearchRequest>,
    search_cancel_outbox: bool,
    next_search_request_id: u64,
    pub git: GitState,
    git_outbox: Vec<GitRequest>,
    next_git_request_id: u64,
    pub lsp: LspState,
    document_symbols: BTreeMap<PathBuf, Vec<DocumentSymbol>>,
    lsp_outbox: Vec<(LspServerId, LspRequest)>,
    next_lsp_request_id: u64,
    next_lsp_server_id: u64,
    pending_workspace_edit: Option<PendingWorkspaceEdit>,
    next_preview_request_id: u64,
    pending_open_location: Option<(PathBuf, usize, usize)>,
    task_service: Box<dyn termesh_tasks::TaskService>,
    task_catalog: Vec<termesh_core::TaskSpec>,
    pub task_runs: Vec<TaskRun>,
    next_task_run_id: u64,
    next_location_request_id: u64,
    pending_problem_navigation: Option<PendingProblemNavigation>,
    problem_cursor: Option<usize>,
    /// Work for the filesystem worker, queued here rather than dispatched directly.
    ///
    /// Keeping the model a pure state machine that *emits* requests (Elm-style commands)
    /// is what lets every explorer behaviour below be tested with no threads, no worker,
    /// and no disk — the loop in `main` is the only thing that owns the channel.
    outbox: Vec<FsRequest>,
}

impl Model {
    pub fn new() -> Self {
        Self {
            running: true,
            focus: Pane::Editor,
            layout: LayoutState::default(),
            overlays: Vec::new(),
            registry: ActionRegistry::with_defaults(),
            keymap: default_keymap(),
            theme: Theme::dark(),
            settings: Settings::default(),
            notification: None,
            first_run: true,
            explorer: None,
            ignore_options: IgnoreOptions::default(),
            buffers: Vec::new(),
            active_buffer: None,
            next_buffer_id: 0,
            opening: Vec::new(),
            highlighter: None,
            find: None,
            agent_name: None,
            pending_prompt: None,
            agent_scroll: 0,
            agent_scroll_max: 0,
            agent: None,
            restored_agent_history: Vec::new(),
            agent_capabilities: None,
            permission_policy: PermissionPolicy::default(),
            pending_terminal_creates: Vec::new(),
            pending_terminal_waits: Vec::new(),
            terminals: Vec::new(),
            active_terminal: None,
            next_terminal_id: 0,
            previous_non_terminal_focus: Pane::Editor,
            terminal_size: TerminalSize { rows: 24, cols: 80 },
            terminal_outbox: Vec::new(),
            terminal_copy_mode: false,
            clipboard_outbox: Vec::new(),
            agent_outbox: Vec::new(),
            served_reads: Vec::new(),
            pending_agent_reads: Vec::new(),
            pending_config_reloads: Vec::new(),
            pending_session_restore: None,
            drafts_dir: termesh_platform::drafts_dir(),
            draft_versions: BTreeMap::new(),
            draft_deadline: None,
            pending_draft_writes: BTreeMap::new(),
            known_drafts: HashSet::new(),
            editor_height: 24,
            search_outbox: Vec::new(),
            search_cancel_outbox: false,
            next_search_request_id: 0,
            git: GitState::default(),
            git_outbox: Vec::new(),
            next_git_request_id: 0,
            lsp: LspState::default(),
            document_symbols: BTreeMap::new(),
            lsp_outbox: Vec::new(),
            next_lsp_request_id: 0,
            next_lsp_server_id: 0,
            pending_workspace_edit: None,
            next_preview_request_id: 0,
            pending_open_location: None,
            task_service: Box::new(termesh_tasks::AdapterTaskService::cargo_only()),
            task_catalog: Vec::new(),
            task_runs: Vec::new(),
            next_task_run_id: 0,
            next_location_request_id: 0,
            pending_problem_navigation: None,
            problem_cursor: None,
            outbox: Vec::new(),
        }
    }

    #[cfg(test)]
    pub fn set_drafts_dir(&mut self, drafts_dir: Option<PathBuf>) {
        self.drafts_dir = drafts_dir;
        self.draft_versions.clear();
        self.draft_deadline = None;
    }

    fn sync_drafts_at(&mut self, now: Instant) {
        let Autosave::Debounced { seconds } = self.settings.autosave else {
            self.draft_versions.clear();
            self.draft_deadline = None;
            return;
        };
        let current = self
            .buffers
            .iter()
            .filter(|buffer| buffer.is_dirty())
            .filter_map(|buffer| buffer.path().map(|path| (path.to_path_buf(), buffer.version().0)))
            .collect::<BTreeMap<_, _>>();
        let changed =
            current.iter().any(|(path, version)| self.draft_versions.get(path) != Some(version));
        self.draft_versions = current;
        if self.draft_versions.is_empty() {
            self.draft_deadline = None;
        } else if changed {
            self.draft_deadline = Some(now + Duration::from_secs(u64::from(seconds)));
        }
    }

    pub fn next_draft_deadline(&self) -> Option<Instant> {
        self.draft_deadline
    }

    /// Turn an expired debounce into ordinary filesystem-worker requests. Encoding is
    /// pure; the actual directory creation and writes remain off the event-loop thread.
    pub fn queue_due_drafts(&mut self, now: Instant) {
        if !self.draft_deadline.is_some_and(|deadline| deadline <= now) {
            return;
        }
        self.draft_deadline = None;
        let Some(drafts_dir) = self.drafts_dir.clone() else { return };
        let drafts = self
            .buffers
            .iter()
            .filter(|buffer| buffer.is_dirty())
            .filter_map(|buffer| {
                Some((buffer.path()?.to_path_buf(), buffer.to_disk_string(), buffer.version().0))
            })
            .collect::<Vec<_>>();
        if drafts.is_empty() {
            return;
        }
        self.outbox.push(FsRequest::CreateDir(drafts_dir.clone()));
        for (source, text, version) in drafts {
            let file = drafts_dir.join(termesh_workspace::drafts::draft_file_name(&source));
            let draft = termesh_workspace::drafts::Draft {
                path: source.clone(),
                saved_at: SystemTime::now(),
                text,
            };
            let contents = match termesh_workspace::drafts::encode_draft(&draft, &file) {
                Ok(contents) => contents,
                Err(error) => {
                    self.append_notification(format!("could not encode recovery draft: {error}"));
                    continue;
                }
            };
            self.next_buffer_id += 1;
            let buffer = BufferId::new(self.next_buffer_id);
            self.pending_draft_writes.insert(buffer, source);
            self.outbox.push(FsRequest::WriteFile { buffer, path: file, contents, version });
        }
    }

    #[cfg(test)]
    pub fn reschedule_drafts_at(&mut self, now: Instant) {
        self.draft_versions.clear();
        self.sync_drafts_at(now);
    }

    /// Load crash drafts without applying them. Source files are read through the same
    /// filesystem service before the recovery choice appears, so disk always wins until
    /// the human explicitly accepts the draft.
    pub fn restore_drafts(&mut self, fs: &dyn FileSystemService) {
        let (Some(drafts_dir), Some(root)) = (
            self.drafts_dir.clone(),
            self.explorer.as_ref().map(|explorer| explorer.root.path.clone()),
        ) else {
            return;
        };
        if let Err(error) = termesh_workspace::drafts::reap_drafts(
            fs,
            &drafts_dir,
            SystemTime::now(),
            termesh_workspace::drafts::RETENTION,
        ) {
            self.append_notification(format!("could not reap recovery drafts: {error}"));
        }
        let (drafts, diagnostics) =
            match termesh_workspace::drafts::drafts_for(fs, &drafts_dir, &root) {
                Ok(result) => result,
                Err(error) => {
                    self.append_notification(format!("could not load recovery drafts: {error}"));
                    return;
                }
            };
        for diagnostic in diagnostics {
            self.append_notification(format!(
                "{}: {} ({})",
                diagnostic.file.display(),
                diagnostic.problem,
                diagnostic.fallback
            ));
        }
        if drafts.is_empty() {
            return;
        }
        self.known_drafts.extend(drafts.iter().map(|draft| draft.path.clone()));
        for draft in &drafts {
            if !self.buffers.iter().any(|buffer| buffer.path() == Some(draft.path.as_path())) {
                self.open_file_sync(fs, draft.path.clone());
            }
        }
        self.overlays.push(Overlay::DraftRecovery(DraftRecoveryOverlay {
            chosen: vec![true; drafts.len()],
            drafts,
            selected: 0,
            previous_focus: self.focus,
        }));
    }

    #[cfg(test)]
    pub fn overlay_is_draft_recovery(&self) -> bool {
        matches!(self.overlays.last(), Some(Overlay::DraftRecovery(_)))
    }

    fn apply_recovery_drafts(&mut self, selected_only: bool) {
        let Some(Overlay::DraftRecovery(recovery)) = self.overlays.pop() else { return };
        self.focus = recovery.previous_focus;
        for (index, draft) in recovery.drafts.into_iter().enumerate() {
            if selected_only && !recovery.chosen[index] {
                continue;
            }
            let Some(buffer) =
                self.buffers.iter_mut().find(|buffer| buffer.path() == Some(draft.path.as_path()))
            else {
                self.append_notification(format!(
                    "could not restore draft because its file is not open: {}",
                    draft.path.display()
                ));
                continue;
            };
            let changes = ChangeSet::replace(
                buffer.text().len_chars(),
                0,
                buffer.text().len_chars(),
                draft.text,
            );
            let transaction = buffer.transaction(changes, EditSource::Replace);
            if let Err(error) = buffer.apply(&transaction) {
                self.append_notification(format!(
                    "could not restore {}: {error}",
                    draft.path.display()
                ));
            }
        }
    }

    fn accept_recovery_drafts(&mut self) {
        self.apply_recovery_drafts(false);
    }

    pub fn accept_selected_recovery_drafts(&mut self) {
        self.apply_recovery_drafts(true);
    }

    pub fn discard_recovery_drafts(&mut self) {
        let Some(Overlay::DraftRecovery(recovery)) = self.overlays.pop() else { return };
        self.focus = recovery.previous_focus;
        let Some(drafts_dir) = self.drafts_dir.clone() else { return };
        for draft in recovery.drafts {
            self.known_drafts.remove(&draft.path);
            self.outbox.push(FsRequest::Remove {
                path: drafts_dir.join(termesh_workspace::drafts::draft_file_name(&draft.path)),
                recursive: false,
            });
        }
    }

    #[cfg(test)]
    pub fn discard_drafts(&mut self, fs: &dyn FileSystemService) {
        let Some(Overlay::DraftRecovery(recovery)) = self.overlays.pop() else { return };
        self.focus = recovery.previous_focus;
        let Some(drafts_dir) = self.drafts_dir.clone() else { return };
        for draft in recovery.drafts {
            let file = drafts_dir.join(termesh_workspace::drafts::draft_file_name(&draft.path));
            if let Err(error) = fs.remove_file(&file) {
                if !matches!(error, termesh_core::FsError::NotFound(_)) {
                    self.append_notification(format!("could not discard recovery draft: {error}"));
                }
            }
        }
    }

    /// Deterministic test seam for the debounced production writer. All I/O still flows
    /// through `FileSystemService`, matching the worker-backed runtime path.
    #[cfg(test)]
    pub fn flush_drafts(&mut self, fs: &dyn FileSystemService) {
        let Some(drafts_dir) = self.drafts_dir.as_deref() else { return };
        for buffer in &self.buffers {
            let Some(path) = buffer.path() else { continue };
            let draft_file = drafts_dir.join(termesh_workspace::drafts::draft_file_name(path));
            if !buffer.is_dirty() {
                match fs.remove_file(&draft_file) {
                    Ok(()) | Err(termesh_core::FsError::NotFound(_)) => {}
                    Err(error) => {
                        self.notification =
                            Some(format!("could not clear recovery draft: {error}"));
                        break;
                    }
                }
                continue;
            }
            let draft = termesh_workspace::drafts::Draft {
                path: path.to_path_buf(),
                saved_at: SystemTime::now(),
                text: buffer.to_disk_string(),
            };
            if let Err(error) = termesh_workspace::drafts::write_draft(fs, drafts_dir, &draft) {
                self.notification = Some(format!("could not write recovery draft: {error}"));
                break;
            }
        }
    }

    /// Load `config.toml`'s text, storing the parsed settings and surfacing every
    /// diagnostic through the notification path (ARCHITECTURE.md §13). The caller reads
    /// the bytes through `FileSystemService`; a missing file is the normal case and is
    /// never passed here as an error.
    pub fn apply_settings(&mut self, text: &str, file: &Path) {
        let (settings, diagnostics) = Settings::parse(text);
        let exclusions_changed = self.settings.exclusions != settings.exclusions;
        self.theme = match settings.theme {
            ThemeChoice::Dark => Theme::for_depth(self.theme.depth()),
        };
        self.settings = settings;
        self.surface_config_diagnostics(file, diagnostics);
        if exclusions_changed {
            self.refresh_loaded_directories();
        }
    }

    pub fn apply_settings_bytes(&mut self, bytes: Vec<u8>, file: &Path) {
        self.apply_config_bytes(ConfigReloadKind::Settings, bytes, file);
    }

    pub fn apply_keymap_bytes(&mut self, bytes: Vec<u8>, file: &Path) {
        self.apply_config_bytes(ConfigReloadKind::Keymap, bytes, file);
    }

    fn apply_config_bytes(&mut self, kind: ConfigReloadKind, bytes: Vec<u8>, file: &Path) {
        match String::from_utf8(bytes) {
            Ok(text) => match kind {
                ConfigReloadKind::Settings => self.apply_settings(&text, file),
                ConfigReloadKind::Keymap => self.apply_keymap(&text, file),
            },
            Err(_) => {
                self.reset_config_with_diagnostic(kind, file, "file is not valid UTF-8".into())
            }
        }
    }

    pub fn apply_settings_read_error(&mut self, file: &Path, error: termesh_core::FsError) {
        self.apply_config_read_error(ConfigReloadKind::Settings, file, error);
    }

    pub fn apply_keymap_read_error(&mut self, file: &Path, error: termesh_core::FsError) {
        self.apply_config_read_error(ConfigReloadKind::Keymap, file, error);
    }

    fn apply_config_read_error(
        &mut self,
        kind: ConfigReloadKind,
        file: &Path,
        error: termesh_core::FsError,
    ) {
        if matches!(error, termesh_core::FsError::NotFound(_)) {
            match kind {
                ConfigReloadKind::Settings => self.apply_settings("", file),
                ConfigReloadKind::Keymap => self.apply_keymap("", file),
            }
        } else {
            self.reset_config_with_diagnostic(kind, file, error.to_string());
        }
    }

    fn reset_config_with_diagnostic(
        &mut self,
        kind: ConfigReloadKind,
        file: &Path,
        problem: String,
    ) {
        match kind {
            ConfigReloadKind::Settings => {
                let exclusions_changed = !self.settings.exclusions.is_empty();
                self.settings = Settings::default();
                self.theme = Theme::for_depth(self.theme.depth());
                if exclusions_changed {
                    self.refresh_loaded_directories();
                }
            }
            ConfigReloadKind::Keymap => self.keymap = default_keymap(),
        }
        self.surface_config_diagnostics(
            file,
            vec![ConfigDiagnostic {
                file: PathBuf::new(),
                line: None,
                problem,
                fallback: match kind {
                    ConfigReloadKind::Settings => "using default settings".into(),
                    ConfigReloadKind::Keymap => "using the default keymap".into(),
                },
            }],
        );
    }

    fn refresh_loaded_directories(&mut self) {
        let Some(explorer) = self.explorer.as_ref() else { return };
        self.outbox.extend(
            explorer
                .tree
                .loaded_directories()
                .into_iter()
                .map(|(id, path)| FsRequest::ReadDir { id, path }),
        );
    }

    /// Rebuild the live keymap from the compiled defaults and overlay `keymap.toml`'s
    /// text on top, then surface every diagnostic through the notification path.
    ///
    /// Always starts from `default_keymap()` rather than overlaying onto whatever is
    /// already live: on `config.reload` the live map already carries the previous
    /// load's overlay, and overlaying again on top would only ever add bindings, never
    /// remove one a user deleted from the file. A parse failure still leaves every
    /// default binding intact (ADR-0014 §3) — it just means the *previous* file's
    /// overlay is gone too, which matches "the file is what's live" for every other
    /// failure mode.
    pub fn apply_keymap(&mut self, text: &str, file: &Path) {
        self.keymap = default_keymap();
        let diagnostics = termesh_config::apply_keymap_file(&mut self.keymap, text);
        self.surface_config_diagnostics(file, diagnostics);
    }

    /// Queue a fresh read of `config.toml` and `keymap.toml` through the filesystem
    /// worker (`Action::ConfigReload`). Both are re-read, not just whichever the user
    /// most recently edited — reloading only one would silently surprise anyone who
    /// just edited the other. A missing file resolves the same way as one that was
    /// never there: back to the compiled defaults.
    fn reload_config(&mut self) {
        if let Some(path) = termesh_platform::config_file() {
            self.queue_config_reload(ConfigReloadKind::Settings, path);
        }
        if let Some(path) = termesh_platform::keymap_file() {
            self.queue_config_reload(ConfigReloadKind::Keymap, path);
        }
    }

    fn queue_config_reload(&mut self, kind: ConfigReloadKind, path: PathBuf) {
        self.next_buffer_id += 1;
        let buffer = BufferId::new(self.next_buffer_id);
        self.pending_config_reloads.push(PendingConfigReload { buffer, kind, path: path.clone() });
        self.outbox.push(FsRequest::ReadFile { buffer, path });
    }

    /// Stamp `file` onto each diagnostic and fold them into one status-bar line. Shared by
    /// every `~/.config/<app>/*.toml` load path so they all report the same way.
    fn surface_config_diagnostics(&mut self, file: &Path, mut diagnostics: Vec<ConfigDiagnostic>) {
        if diagnostics.is_empty() {
            return;
        }
        for diagnostic in &mut diagnostics {
            diagnostic.file = file.to_path_buf();
        }
        let name = file.file_name().map(|n| n.to_string_lossy().into_owned());
        let summary = diagnostics
            .iter()
            .map(|d| {
                format!("{}: {} ({})", name.as_deref().unwrap_or("config"), d.problem, d.fallback)
            })
            .collect::<Vec<_>>()
            .join("; ");
        self.notification = Some(summary);
    }

    /// Which keymap context the next chord resolves in (ADR: `KeyContext`).
    ///
    /// Replaces Phase 02's focus check inside the command handlers: the pane decides what
    /// a chord *means*, once, in resolution — rather than every handler re-asking whether
    /// it is allowed to run.
    pub fn key_context(&self) -> KeyContext {
        match self.focus {
            Pane::Project => KeyContext::Project,
            Pane::Editor if self.active_buffer.is_some() => KeyContext::Editor,
            // Not gated on a session existing: pressing Enter here is how you *start*
            // one, and requiring a session to reach the key that creates a session is a
            // deadlock the user experiences as "nothing happens".
            Pane::Agent => KeyContext::Agent,
            Pane::Terminal => KeyContext::Terminal,
            _ => KeyContext::Global,
        }
    }

    /// Tell the model how tall the editor's text area is, in rows.
    pub fn set_editor_height(&mut self, rows: usize) {
        self.editor_height = rows;
    }

    pub fn active_buffer(&self) -> Option<&Buffer> {
        self.active_buffer.and_then(|i| self.buffers.get(i))
    }

    pub fn active_buffer_mut(&mut self) -> Option<&mut Buffer> {
        self.active_buffer.and_then(|i| self.buffers.get_mut(i))
    }

    /// Open a file in the editor, or focus it if it is already open.
    ///
    /// The read goes to the worker: a cold file on a network mount must not freeze the
    /// UI any more than a cold directory does (ADR-0005 §1).
    pub fn open_file(&mut self, path: PathBuf) {
        if let Some(i) = self.buffers.iter().position(|b| b.path() == Some(path.as_path())) {
            self.active_buffer = Some(i);
            self.focus = Pane::Editor;
            return;
        }
        if self.opening.iter().any(|(_, p)| *p == path) {
            return; // already in flight
        }

        self.next_buffer_id += 1;
        let buffer = BufferId::new(self.next_buffer_id);
        self.opening.push((buffer, path.clone()));
        self.outbox.push(FsRequest::ReadFile { buffer, path });
    }

    pub fn open_file_at(&mut self, path: PathBuf, line: usize, column: usize) {
        if self.position_open_buffer(&path, line, column) {
            return;
        }
        self.pending_open_location = Some((path.clone(), line, column));
        self.open_file(path);
    }

    fn position_open_buffer(&mut self, path: &Path, line: usize, column: usize) -> bool {
        let Some(index) = self.buffers.iter().position(|buffer| buffer.path() == Some(path)) else {
            return false;
        };
        let buffer = &mut self.buffers[index];
        let line_index = line.saturating_sub(1).min(buffer.text().len_lines().saturating_sub(1));
        let (start, end) = buffer.line_range(line_index);
        let position = start.saturating_add(column.saturating_sub(1)).min(end);
        buffer.set_selection(termesh_editor::Selection::point(position));
        buffer.scroll_to_cursor(self.editor_height);
        self.active_buffer = Some(index);
        self.focus = Pane::Editor;
        true
    }

    /// Write the active buffer back through the worker.
    fn save_active_buffer(&mut self) {
        let Some(b) = self.active_buffer() else {
            self.notification = Some("nothing to save".into());
            return;
        };
        let Some(path) = b.path().map(Path::to_path_buf) else {
            self.notification = Some("buffer has no path".into());
            return;
        };
        if !b.is_dirty() {
            self.notification = Some("no changes to save".into());
            return;
        }
        // The version travels with the bytes so the buffer only clears its dirty flag for
        // what actually reached disk — the user may type again before the write returns.
        self.outbox.push(FsRequest::WriteFile {
            buffer: b.id(),
            path,
            contents: b.to_disk_string().into_bytes(),
            version: b.version().0,
        });
    }

    fn buffer_mut(&mut self, id: BufferId) -> Option<&mut Buffer> {
        self.buffers.iter_mut().find(|b| b.id() == id)
    }

    /// Run an edit on the active buffer, surfacing any refusal rather than swallowing it.
    fn edit_active(&mut self, f: impl FnOnce(&mut Buffer) -> termesh_editor::EditResult<()>) {
        let height = self.editor_height;
        let Some(buffer) = self.active_buffer_mut() else { return };
        let outcome = f(buffer);
        // An edit moves the cursor too — a newline at the bottom of the viewport has to
        // bring the next line into view.
        buffer.scroll_to_cursor(height);
        if let Err(e) = outcome {
            self.notification = Some(e.to_string());
        }
    }

    /// Open a detected workspace root, expanding it so the first level loads at once.
    ///
    /// **Order matters.** `Watch` is queued *before* the first `ReadDir`: it is what tells
    /// the worker the root, and therefore what anchors the ignore chain. Queued the other
    /// way round, the worker serves the very first listing — the one the user sees on
    /// launch — with no ignore rules, and nothing re-reads the root to correct it.
    pub fn open_workspace(&mut self, root: WorkspaceRoot) {
        self.open_workspace_with_language(root, LanguageSettings::default(), Vec::new());
    }

    /// Open a workspace with the raw command override resolved at the application
    /// boundary. Neither `ProjectKind` nor workspace config crosses into `termesh-lsp`.
    fn open_workspace_with_language(
        &mut self,
        root: WorkspaceRoot,
        language_settings: LanguageSettings,
        task_catalog: Vec<termesh_core::TaskSpec>,
    ) {
        self.first_run = false;
        for server in self.lsp.sessions.keys().copied().collect::<Vec<_>>() {
            self.lsp_outbox.push((server, LspRequest::Shutdown));
        }
        self.lsp = LspState::default();

        for kind in &root.kinds {
            let label = kind.label();
            let command_override = language_settings.command(label).cloned();
            let Some(recipe) = termesh_lsp::resolve_recipe(label, command_override) else {
                continue;
            };
            let launch = SessionLaunch {
                root: root.path.clone(),
                command: recipe.command,
                initialization_options: recipe.initialization_options,
            };
            self.lsp.configured.push(ConfiguredRecipe::new(
                recipe.language_id,
                recipe.extensions,
                launch,
            ));
        }

        self.task_catalog = task_catalog;
        let mut tree = FileTree::new(root.path.clone(), root.display_name());
        self.outbox.push(FsRequest::Watch(root.path.clone()));
        if let Some(path) = tree.expand(tree.root()) {
            self.outbox.push(FsRequest::ReadDir { id: tree.root(), path });
        }
        self.focus = Pane::Project;
        self.explorer = Some(Explorer { root, tree });
        self.git = GitState::default();
        self.git_outbox.clear();
        self.request_git_refresh();
    }

    /// What the agent is told about the workspace (ARCHITECTURE.md §9.2).
    ///
    /// Built from the same `FileTree` the human is looking at, so the agent's view and
    /// the screen cannot disagree — that shared state is the whole premise of the
    /// project. Phase 03 attaches this to the ACP turn.
    pub fn workspace_snapshot(&self) -> Option<WorkspaceSnapshot> {
        let e = self.explorer.as_ref()?;
        Some(WorkspaceSnapshot::build(&e.root, &e.tree))
    }

    /// Drain queued filesystem work. The main loop calls this after every update and
    /// forwards the result to the worker.
    pub fn take_fs_requests(&mut self) -> Vec<FsRequest> {
        std::mem::take(&mut self.outbox)
    }

    /// Drain search work for the background worker.
    pub fn take_search_requests(&mut self) -> Vec<SearchRequest> {
        std::mem::take(&mut self.search_outbox)
    }

    pub fn take_search_cancel(&mut self) -> bool {
        std::mem::take(&mut self.search_cancel_outbox)
    }

    pub fn take_git_requests(&mut self) -> Vec<GitRequest> {
        std::mem::take(&mut self.git_outbox)
    }

    pub fn take_lsp_requests(&mut self) -> Vec<(LspServerId, LspRequest)> {
        std::mem::take(&mut self.lsp_outbox)
    }

    /// Synchronise every open buffer and always drain its transaction outbox. Documents
    /// no configured session claims are deliberately discarded rather than accumulated.
    pub fn sync_lsp_documents(&mut self) {
        for index in 0..self.buffers.len() {
            let (path, text, pending) = {
                let buffer = &mut self.buffers[index];
                let path = buffer.path().map(Path::to_path_buf);
                let text = buffer.text().clone();
                let pending = buffer.take_pending_changes();
                if !pending.is_empty() {
                    buffer.decorations_mut().clear_diagnostics();
                }
                (path, text, pending)
            };
            let Some(path) = path else { continue };
            if !pending.is_empty() {
                self.lsp.diagnostics.remove(&path);
            }
            let server = match self.lsp.server_for(&path) {
                Some(server) => server,
                None => {
                    let Some(recipe_index) = self.lsp.recipe_for_path(&path) else { continue };
                    let recipe = self.lsp.configured.remove(recipe_index);
                    self.next_lsp_server_id += 1;
                    let server = LspServerId::new(self.next_lsp_server_id);
                    self.lsp.sessions.insert(
                        server,
                        LspSessionState::new(
                            server,
                            recipe.language.clone(),
                            recipe.extensions,
                            recipe.launch.clone(),
                        ),
                    );
                    self.lsp_outbox.push((
                        server,
                        LspRequest::Start {
                            server,
                            root: recipe.launch.root,
                            command: recipe.launch.command,
                            language: recipe.language,
                            initialization_options: recipe.launch.initialization_options,
                        },
                    ));
                    server
                }
            };
            let Some(session) = self.lsp.sessions.get_mut(&server) else { continue };

            if !session.open_docs.contains_key(&path) {
                let version = session.next_document_version();
                session.open_docs.insert(path.clone(), version);
                session.synced_docs.insert(path.clone(), text.clone());
                self.lsp_outbox.push((
                    server,
                    LspRequest::DidOpen {
                        path,
                        language_id: session.language.clone(),
                        version,
                        text: text.to_string(),
                    },
                ));
                // `didOpen` contains the current post-image, including anything that
                // happened before the session owned the document.
                continue;
            }

            let mut shadow = session.synced_docs.remove(&path).unwrap_or_else(|| text.clone());
            let mut fell_out_of_sync = false;
            for changes in pending {
                if changes.len_before() != shadow.len_chars() {
                    fell_out_of_sync = true;
                    break;
                }
                let Some(span) = changes.changed_span() else { continue };
                let post_image = changes.apply(&shadow);
                let (start_line, start_character) =
                    termesh_editor::position::utf16_position(&shadow, span.before_start);
                let (end_line, end_character) =
                    termesh_editor::position::utf16_position(&shadow, span.before_end);
                let replacement = post_image.slice(span.after_start..span.after_end).to_string();
                let version = session.next_document_version();
                session.open_docs.insert(path.clone(), version);
                self.lsp_outbox.push((
                    server,
                    LspRequest::DidChange {
                        path: path.clone(),
                        version,
                        change: TextChange {
                            range: Some(TextRange {
                                start: TextPosition {
                                    line: start_line,
                                    character: start_character,
                                },
                                end: TextPosition { line: end_line, character: end_character },
                            }),
                            text: replacement,
                        },
                    },
                ));
                shadow = post_image;
            }

            if fell_out_of_sync || shadow != text {
                // Refuse to send a range against the wrong pre-image. A close/open pair
                // is the protocol-safe recovery and keeps wire versions monotonic.
                self.lsp_outbox.push((server, LspRequest::DidClose { path: path.clone() }));
                let version = session.next_document_version();
                session.open_docs.insert(path.clone(), version);
                self.lsp_outbox.push((
                    server,
                    LspRequest::DidOpen {
                        path: path.clone(),
                        language_id: session.language.clone(),
                        version,
                        text: text.to_string(),
                    },
                ));
                shadow = text;
            }
            session.synced_docs.insert(path, shadow);
        }
        self.sync_drafts_at(Instant::now());
    }

    /// Relaunch every language server with the recipe resolved when the workspace opened.
    ///
    /// The session keeps its `LspServerId`: the main loop replaces the process bound to
    /// that id, so a restart cannot orphan the session the model is still routing to.
    /// Open documents resynchronise on their own — `dispatch` runs `sync_lsp_documents`
    /// after this, and a reset session no longer lists them as open.
    fn restart_language_servers(&mut self) {
        if self.lsp.sessions.is_empty() {
            self.notification = Some(if self.lsp.configured.is_empty() {
                "No language server is configured for this workspace".to_string()
            } else {
                "No language server has started yet".to_string()
            });
            return;
        }

        for server in self.lsp.sessions.keys().copied().collect::<Vec<_>>() {
            let Some(session) = self.lsp.sessions.get_mut(&server) else { continue };
            session.reset_for_restart();
            let launch = session.launch.clone();
            let language = session.language.clone();
            self.lsp_outbox.push((
                server,
                LspRequest::Start {
                    server,
                    root: launch.root,
                    command: launch.command,
                    language,
                    initialization_options: launch.initialization_options,
                },
            ));
        }

        // Diagnostics and symbols belong to the process that produced them; the
        // replacement republishes from its own analysis.
        self.lsp.diagnostics.clear();
        self.document_symbols.clear();
        for buffer in &mut self.buffers {
            buffer.decorations_mut().clear_diagnostics();
        }
        self.notification = Some("Restarting the language server".to_string());
    }

    fn close_lsp_document(&mut self, path: &Path) {
        let Some(server) = self.lsp.server_for(path) else { return };
        let Some(session) = self.lsp.sessions.get_mut(&server) else { return };
        if session.open_docs.remove(path).is_some() {
            session.synced_docs.remove(path);
            self.lsp_outbox.push((server, LspRequest::DidClose { path: path.to_path_buf() }));
        }
        self.lsp.diagnostics.remove(path);
        self.document_symbols.remove(path);
    }

    fn sync_lsp_diagnostics(&mut self, path: &Path) {
        let diagnostics = self.lsp.diagnostics.get(path).cloned().unwrap_or_default();
        let Some(buffer) = self.buffers.iter_mut().find(|buffer| buffer.path() == Some(path))
        else {
            return;
        };
        let decorations: Vec<_> = diagnostics
            .into_iter()
            .map(|diagnostic| {
                let start = termesh_editor::position::offset_from_utf16(
                    buffer.text(),
                    diagnostic.range.start.line,
                    diagnostic.range.start.character,
                );
                let end = termesh_editor::position::offset_from_utf16(
                    buffer.text(),
                    diagnostic.range.end.line,
                    diagnostic.range.end.character,
                );
                let severity = match diagnostic.severity {
                    termesh_core::DiagnosticSeverity::Error => termesh_editor::Severity::Error,
                    termesh_core::DiagnosticSeverity::Warning => termesh_editor::Severity::Warning,
                    termesh_core::DiagnosticSeverity::Info => termesh_editor::Severity::Info,
                    termesh_core::DiagnosticSeverity::Hint => termesh_editor::Severity::Hint,
                };
                Decoration::new(start, end, DecorationClass::Diagnostic(severity))
            })
            .collect();
        buffer.decorations_mut().clear_diagnostics();
        for decoration in decorations {
            buffer.decorations_mut().push(decoration);
        }
    }

    fn notify_lsp_watched_files(&mut self, paths: &[PathBuf]) {
        let Some(root) = self.explorer.as_ref().map(|explorer| explorer.root.path.clone()) else {
            return;
        };
        let changed_paths: Vec<_> =
            paths.iter().filter(|path| path.starts_with(&root)).cloned().collect();
        if changed_paths.is_empty() {
            return;
        }
        let changes: Vec<_> =
            changed_paths.iter().cloned().map(WatchedFileChange::Changed).collect();
        let reload_paths: Vec<_> =
            changed_paths.iter().filter(|path| is_java_build_file(path)).cloned().collect();
        let sessions: Vec<_> = self
            .lsp
            .sessions
            .iter()
            .map(|(server, session)| (*server, session.language == "java"))
            .collect();
        for (server, is_java) in sessions {
            self.lsp_outbox
                .push((server, LspRequest::WatchedFilesChanged { changes: changes.clone() }));
            if is_java && !reload_paths.is_empty() {
                self.lsp_outbox
                    .push((server, LspRequest::ReloadProject { paths: reload_paths.clone() }));
            }
        }
    }

    pub fn on_lsp_event(&mut self, server: LspServerId, event: LspEvent) {
        let Some(session) = self.lsp.sessions.get_mut(&server) else { return };
        match event {
            LspEvent::Started => session.load = LspLoadState::Starting,
            LspEvent::Ready => session.load = LspLoadState::Ready,
            LspEvent::Indexing { message, percent } => {
                session.load = LspLoadState::Indexing { message, percent };
            }
            LspEvent::Diagnostics { path, version, items } => {
                if version.is_some_and(|version| session.open_docs.get(&path) != Some(&version)) {
                    return;
                }
                self.lsp.diagnostics.insert(path.clone(), items);
                self.sync_lsp_diagnostics(&path);
            }
            LspEvent::Definition { id, locations } => {
                if session.active_definition != Some(id) {
                    return;
                }
                session.active_definition = None;
                if let Some(location) = locations.into_iter().next() {
                    self.open_lsp_location(location);
                } else {
                    self.notification = Some("No definition found".into());
                }
            }
            LspEvent::Hover { id, hover } => {
                if session.active_hover != Some(id) {
                    return;
                }
                session.active_hover = None;
                if let Some(hover) = hover {
                    self.overlays
                        .push(Overlay::Hover(HoverOverlay { hover, previous_focus: self.focus }));
                } else {
                    self.notification = Some("No hover information".into());
                }
            }
            LspEvent::Completion { id, items } => {
                if session.active_completion != Some(id) {
                    return;
                }
                session.active_completion = None;
                if items.is_empty() {
                    self.notification = Some("No completions".into());
                } else {
                    self.overlays.push(Overlay::Completion(CompletionOverlay {
                        items,
                        selected: 0,
                        previous_focus: self.focus,
                    }));
                }
            }
            LspEvent::References { id, locations } => {
                if session.active_references != Some(id) {
                    return;
                }
                session.active_references = None;
                if locations.is_empty() {
                    self.notification = Some("No references found".into());
                } else {
                    self.overlays.push(Overlay::References(ReferencesOverlay {
                        locations,
                        selected: 0,
                        previous_focus: self.focus,
                    }));
                }
            }
            LspEvent::DocumentSymbols { id, symbols } => {
                if session.active_document_symbols != Some(id) {
                    return;
                }
                session.active_document_symbols = None;
                let path = session.active_document_symbol_path.take();
                let Some(path) = path else { return };
                self.document_symbols.insert(path.clone(), symbols.clone());
                let mut rows = Vec::new();
                flatten_document_symbols(&symbols, &path, 0, &mut rows);
                if rows.is_empty() {
                    self.notification = Some("No document symbols".into());
                } else {
                    self.overlays.push(Overlay::Symbols(SymbolsOverlay {
                        title: "Document Symbols".into(),
                        rows,
                        selected: 0,
                        previous_focus: self.focus,
                    }));
                }
            }
            LspEvent::WorkspaceSymbols { id, symbols } => {
                if session.active_workspace_symbols != Some(id) {
                    return;
                }
                session.active_workspace_symbols = None;
                if let Some(Overlay::Symbols(overlay)) = self.overlays.last_mut() {
                    overlay.rows.extend(symbols.into_iter().map(|symbol| SymbolRow {
                        label: symbol.name,
                        detail: symbol.container,
                        depth: 0,
                        location: symbol.location,
                    }));
                    overlay.rows.sort_by(|left, right| {
                        left.label.to_lowercase().cmp(&right.label.to_lowercase())
                    });
                }
            }
            LspEvent::Rename { id, edit } => {
                if session.active_rename != Some(id) {
                    return;
                }
                session.active_rename = None;
                self.apply_workspace_edit(edit);
            }
            LspEvent::CodeActions { id, actions } => {
                if session.active_code_actions != Some(id) {
                    return;
                }
                session.active_code_actions = None;
                if actions.is_empty() {
                    self.notification = Some("No code actions available".into());
                } else {
                    self.overlays.push(Overlay::CodeActions(CodeActionOverlay {
                        actions,
                        selected: 0,
                        previous_focus: self.focus,
                    }));
                }
            }
            LspEvent::Formatting { id, edits } => {
                if session.active_formatting != Some(id) {
                    return;
                }
                session.active_formatting = None;
                self.apply_text_edits(edits);
            }
            LspEvent::Failed { id, failure } => {
                if id.is_some_and(|id| !session.clear_request(id)) {
                    return;
                }
                session.load = if id.is_none() {
                    LspLoadState::Unavailable(failure.clone())
                } else {
                    LspLoadState::Stale(failure.clone())
                };
                self.notification = Some(format!("{}: {}", session.language, failure.message));
            }
            LspEvent::Unavailable { message } => {
                let failure = LspFailure { kind: LspFailureKind::Transport, message };
                session.load = LspLoadState::Unavailable(failure.clone());
                self.notification = Some(format!("{}: {}", session.language, failure.message));
            }
            LspEvent::Exited { code } => {
                let failure = LspFailure {
                    kind: LspFailureKind::Server,
                    message: match code {
                        Some(code) => format!("language server exited with status {code}"),
                        None => "language server exited".into(),
                    },
                };
                session.load = LspLoadState::Stale(failure);
            }
        }
    }

    fn lsp_cursor_target(&self) -> Option<(LspServerId, PathBuf, TextPosition)> {
        let buffer = self.active_buffer()?;
        let path = buffer.path().map(Path::to_path_buf)?;
        let server = self.lsp.server_for(&path)?;
        let offset = buffer.selection().primary().head;
        let (line, character) = termesh_editor::position::utf16_position(buffer.text(), offset);
        Some((server, path, TextPosition { line, character }))
    }

    fn next_lsp_request(&mut self) -> LspRequestId {
        self.next_lsp_request_id += 1;
        LspRequestId::new(self.next_lsp_request_id)
    }

    fn request_lsp_definition(&mut self) {
        let Some((server, path, position)) = self.lsp_cursor_target() else { return };
        let id = self.next_lsp_request();
        let Some(session) = self.lsp.sessions.get_mut(&server) else { return };
        if let Some(previous) = session.active_definition.replace(id) {
            self.lsp_outbox.push((server, LspRequest::Cancel { id: previous }));
        }
        self.lsp_outbox.push((server, LspRequest::Definition { id, path, position }));
    }

    fn request_lsp_hover(&mut self) {
        let Some((server, path, position)) = self.lsp_cursor_target() else { return };
        let id = self.next_lsp_request();
        let Some(session) = self.lsp.sessions.get_mut(&server) else { return };
        if let Some(previous) = session.active_hover.replace(id) {
            self.lsp_outbox.push((server, LspRequest::Cancel { id: previous }));
        }
        self.lsp_outbox.push((server, LspRequest::Hover { id, path, position }));
    }

    fn request_lsp_completion(&mut self) {
        let Some((server, path, position)) = self.lsp_cursor_target() else { return };
        let id = self.next_lsp_request();
        let Some(session) = self.lsp.sessions.get_mut(&server) else { return };
        if let Some(previous) = session.active_completion.replace(id) {
            self.lsp_outbox.push((server, LspRequest::Cancel { id: previous }));
        }
        self.lsp_outbox.push((server, LspRequest::Completion { id, path, position }));
    }

    /// Request formatting for the active document, superseding an older request.
    pub fn format_document(&mut self) {
        let Some(buffer) = self.active_buffer() else { return };
        let Some(path) = buffer.path().map(Path::to_path_buf) else { return };
        let Some(server) = self.lsp.server_for(&path) else { return };
        let id = self.next_lsp_request();
        let Some(session) = self.lsp.sessions.get_mut(&server) else { return };
        if let Some(previous) = session.active_formatting.replace(id) {
            self.lsp_outbox.push((server, LspRequest::Cancel { id: previous }));
        }
        self.lsp_outbox.push((server, LspRequest::Formatting { id, path }));
    }

    fn prompt_lsp_rename(&mut self) {
        if self.lsp_cursor_target().is_none() {
            self.notification = Some("No language server is available for this document".into());
            return;
        }
        self.overlays.push(Overlay::Prompt(Prompt {
            title: "Code: Rename Symbol".into(),
            input: String::new(),
            kind: PromptKind::LspRename,
        }));
    }

    fn request_lsp_rename(&mut self, new_name: String) {
        let Some((server, path, position)) = self.lsp_cursor_target() else { return };
        let id = self.next_lsp_request();
        let Some(session) = self.lsp.sessions.get_mut(&server) else { return };
        if let Some(previous) = session.active_rename.replace(id) {
            self.lsp_outbox.push((server, LspRequest::Cancel { id: previous }));
        }
        self.lsp_outbox.push((server, LspRequest::Rename { id, path, position, new_name }));
    }

    fn request_lsp_code_actions(&mut self) {
        let Some(buffer) = self.active_buffer() else { return };
        let Some(path) = buffer.path().map(Path::to_path_buf) else { return };
        let Some(server) = self.lsp.server_for(&path) else { return };
        let selected = buffer.selection().primary();
        let (start_line, start_character) =
            termesh_editor::position::utf16_position(buffer.text(), selected.start());
        let (end_line, end_character) =
            termesh_editor::position::utf16_position(buffer.text(), selected.end());
        let range = TextRange {
            start: TextPosition { line: start_line, character: start_character },
            end: TextPosition { line: end_line, character: end_character },
        };
        let id = self.next_lsp_request();
        let Some(session) = self.lsp.sessions.get_mut(&server) else { return };
        if let Some(previous) = session.active_code_actions.replace(id) {
            self.lsp_outbox.push((server, LspRequest::Cancel { id: previous }));
        }
        self.lsp_outbox.push((server, LspRequest::CodeActions { id, path, range }));
    }

    /// Apply a server-authored edit set to one open buffer as one undoable transaction.
    ///
    /// All protocol ranges are converted against the same current pre-image. Validation
    /// finishes before the transaction is built, so a malformed response cannot partly
    /// mutate a document.
    pub fn apply_text_edits(&mut self, edits: Vec<TextEdit>) {
        let Some(path) = edits.first().map(|edit| edit.path.clone()) else { return };
        if edits.iter().any(|edit| edit.path != path) {
            self.notification = Some(
                "language server returned edits for multiple files in a single-file response"
                    .into(),
            );
            return;
        }
        let Some(index) = self.buffers.iter().position(|buffer| buffer.path() == Some(&path))
        else {
            self.notification = Some(format!(
                "language server edit targets a file that is not open: {}",
                path.display()
            ));
            return;
        };

        let buffer = &mut self.buffers[index];
        let changes = match build_lsp_changes(buffer, &edits) {
            Ok(changes) => changes,
            Err(message) => {
                self.notification = Some(message);
                return;
            }
        };
        let transaction = buffer.transaction(changes, EditSource::Lsp);
        if let Err(error) = buffer.apply(&transaction) {
            self.notification = Some(error.to_string());
            return;
        }
        self.sync_syntax();
        self.sync_proposals();
        self.sync_lsp_documents();
    }

    /// Accept the selected completion using its explicit edit when supplied, otherwise
    /// inserting the server's insertion text at the current cursor.
    pub fn accept_completion(&mut self, item: CompletionItem) {
        if let Some(edit) = item.edit {
            self.apply_text_edits(vec![edit]);
            return;
        }
        let Some(buffer) = self.active_buffer() else { return };
        let Some(path) = buffer.path().map(Path::to_path_buf) else { return };
        let offset = buffer.selection().primary().head;
        let (line, character) = termesh_editor::position::utf16_position(buffer.text(), offset);
        self.apply_text_edits(vec![TextEdit {
            path,
            range: TextRange {
                start: TextPosition { line, character },
                end: TextPosition { line, character },
            },
            new_text: item.insert_text,
        }]);
    }

    pub fn accept_code_action(&mut self, action: CodeAction) {
        match action.edit {
            Some(edit) => self.apply_workspace_edit(edit),
            None => {
                self.notification =
                    Some(format!("Code action '{}' has no edit Termesh can apply", action.title));
            }
        }
    }

    /// Queue a workspace edit until every target has an open buffer, then apply one
    /// transaction per file. Loading and validation complete before any mutation.
    pub fn apply_workspace_edit(&mut self, edit: WorkspaceEdit) {
        if self.pending_workspace_edit.is_some() {
            self.notification =
                Some("Another language-server workspace edit is still loading".into());
            return;
        }
        if edit.edits.is_empty() {
            self.notification = Some("The selected language action made no changes".into());
            return;
        }
        let Some(root) = self.explorer.as_ref().map(|explorer| explorer.root.path.clone()) else {
            self.notification = Some("A workspace must be open to apply a workspace edit".into());
            return;
        };
        let paths: std::collections::BTreeSet<_> =
            edit.edits.iter().map(|item| item.path.clone()).collect();
        if let Some(path) = paths.iter().find(|path| !path.starts_with(&root)) {
            self.notification =
                Some(format!("Language-server edit is outside the workspace: {}", path.display()));
            return;
        }
        for (path, expected) in &edit.versions {
            let current = self
                .lsp
                .server_for(path)
                .and_then(|server| self.lsp.sessions.get(&server))
                .and_then(|session| session.open_docs.get(path));
            if current != Some(expected) {
                self.notification = Some(format!(
                    "{} changed after the language server prepared this edit; no files were changed",
                    path.display()
                ));
                return;
            }
        }

        let previous_active = self.active_buffer().map(Buffer::id);
        let mut waiting = BTreeMap::new();
        for path in paths {
            if self.buffers.iter().any(|buffer| buffer.path() == Some(&path)) {
                continue;
            }
            if let Some((buffer, _)) = self.opening.iter().find(|(_, pending)| *pending == path) {
                waiting.insert(*buffer, path);
                continue;
            }
            self.next_buffer_id += 1;
            let buffer = BufferId::new(self.next_buffer_id);
            self.opening.push((buffer, path.clone()));
            waiting.insert(buffer, path.clone());
            self.outbox.push(FsRequest::ReadFile { buffer, path });
        }
        self.pending_workspace_edit = Some(PendingWorkspaceEdit { edit, waiting, previous_active });
        self.finish_pending_workspace_edit();
    }

    fn finish_pending_workspace_edit(&mut self) {
        if !self.pending_workspace_edit.as_ref().is_some_and(|pending| pending.waiting.is_empty()) {
            return;
        }
        let pending = self.pending_workspace_edit.take().unwrap();
        let mut grouped: BTreeMap<PathBuf, Vec<TextEdit>> = BTreeMap::new();
        for edit in pending.edit.edits {
            grouped.entry(edit.path.clone()).or_default().push(edit);
        }

        let mut prepared = Vec::with_capacity(grouped.len());
        for (path, edits) in &grouped {
            let Some(index) = self.buffers.iter().position(|buffer| buffer.path() == Some(path))
            else {
                self.restore_active_buffer(pending.previous_active);
                self.notification = Some(format!(
                    "Could not open {} for the language-server edit; no files were changed",
                    path.display()
                ));
                return;
            };
            match build_lsp_changes(&self.buffers[index], edits) {
                Ok(changes) => prepared.push((index, changes)),
                Err(message) => {
                    self.restore_active_buffer(pending.previous_active);
                    self.notification = Some(format!("{message}; no files were changed"));
                    return;
                }
            }
        }

        for (index, changes) in prepared {
            let buffer = &mut self.buffers[index];
            let transaction = buffer.transaction(changes, EditSource::Lsp);
            if let Err(error) = buffer.apply(&transaction) {
                // This is unreachable after preflight in the single-owner model: the
                // buffer cannot change between validation and this loop.
                self.notification = Some(format!("Language-server edit failed: {error}"));
                return;
            }
        }
        let file_count = grouped.len();
        self.restore_active_buffer(pending.previous_active);
        self.sync_syntax();
        self.sync_proposals();
        self.sync_lsp_documents();
        self.notification = Some(format!(
            "Applied language-server edit to {file_count} {}",
            if file_count == 1 { "file" } else { "files" }
        ));
    }

    fn restore_active_buffer(&mut self, buffer: Option<BufferId>) {
        if let Some(buffer) = buffer {
            if let Some(index) = self.buffers.iter().position(|candidate| candidate.id() == buffer)
            {
                self.active_buffer = Some(index);
            }
        }
    }

    fn abandon_pending_workspace_edit(&mut self, message: String) {
        if let Some(pending) = self.pending_workspace_edit.take() {
            self.restore_active_buffer(pending.previous_active);
        }
        self.notification = Some(message);
    }

    fn request_lsp_references(&mut self) {
        let Some((server, path, position)) = self.lsp_cursor_target() else { return };
        let id = self.next_lsp_request();
        let Some(session) = self.lsp.sessions.get_mut(&server) else { return };
        if let Some(previous) = session.active_references.replace(id) {
            self.lsp_outbox.push((server, LspRequest::Cancel { id: previous }));
        }
        self.lsp_outbox.push((server, LspRequest::References { id, path, position }));
    }

    fn request_lsp_document_symbols(&mut self) {
        let Some(buffer) = self.active_buffer() else { return };
        let Some(path) = buffer.path().map(Path::to_path_buf) else { return };
        let Some(server) = self.lsp.server_for(&path) else { return };
        let id = self.next_lsp_request();
        let Some(session) = self.lsp.sessions.get_mut(&server) else { return };
        if let Some(previous) = session.active_document_symbols.replace(id) {
            self.lsp_outbox.push((server, LspRequest::Cancel { id: previous }));
        }
        session.active_document_symbol_path = Some(path.clone());
        self.lsp_outbox.push((server, LspRequest::DocumentSymbols { id, path }));
    }

    fn request_lsp_workspace_symbols(&mut self) {
        self.overlays.push(Overlay::Symbols(SymbolsOverlay {
            title: "Workspace Symbols".into(),
            rows: Vec::new(),
            selected: 0,
            previous_focus: self.focus,
        }));
        let servers: Vec<_> = self.lsp.sessions.keys().copied().collect();
        for server in servers {
            let id = self.next_lsp_request();
            let Some(session) = self.lsp.sessions.get_mut(&server) else { continue };
            if let Some(previous) = session.active_workspace_symbols.replace(id) {
                self.lsp_outbox.push((server, LspRequest::Cancel { id: previous }));
            }
            self.lsp_outbox
                .push((server, LspRequest::WorkspaceSymbols { id, query: String::new() }));
        }
    }

    pub(crate) fn open_lsp_location(&mut self, location: termesh_core::Location) {
        if !self
            .explorer
            .as_ref()
            .is_some_and(|explorer| location.path.starts_with(&explorer.root.path))
        {
            self.notification = Some(format!(
                "language-server location is outside workspace: {}",
                location.path.display()
            ));
            return;
        }
        self.open_file_at(
            location.path,
            location.range.start.line as usize + 1,
            location.range.start.character as usize + 1,
        );
    }

    pub fn request_git_refresh(&mut self) {
        let Some(root) = self.explorer.as_ref().map(|explorer| explorer.root.path.clone()) else {
            return;
        };
        if self.git.active_refresh.is_some() {
            self.git.pending_refresh = true;
            return;
        }
        self.next_git_request_id += 1;
        let id = GitRequestId::new(self.next_git_request_id);
        self.git.active_refresh = Some(id);
        self.git.load = GitLoadState::Loading;
        self.git_outbox.push(GitRequest::Refresh { id, root });
    }

    pub fn on_git_event(&mut self, event: GitEvent) {
        match event {
            GitEvent::Started { .. } => {}
            GitEvent::SnapshotLoaded { id, snapshot } => {
                if self.git.active_refresh != Some(id) {
                    return;
                }
                self.refresh_git_status_overlay(&snapshot);
                self.git.snapshot = Some(snapshot);
                self.git.load = GitLoadState::Ready;
                self.finish_git_refresh();
            }
            GitEvent::DiffLoaded { id, diff } => {
                if self.git.active_diff != Some(id) {
                    return;
                }
                if let Some(Overlay::GitDiff(overlay)) = self.overlays.last_mut() {
                    if overlay.path == diff.path && overlay.target == diff.target {
                        overlay.text = Some(diff.text.clone());
                        overlay.truncated = diff.truncated;
                        overlay.error = None;
                        overlay.scroll = 0;
                    }
                }
                self.git.diff = Some(diff);
                self.git.active_diff = None;
            }
            GitEvent::BranchesLoaded { id, branches } => {
                if self.git.active_branches != Some(id) {
                    return;
                }
                self.git.branches = branches;
                self.git.active_branches = None;
                if self.git.branches.is_empty() {
                    self.notification = Some("No local Git branches are available".into());
                } else {
                    self.overlays.push(Overlay::GitBranches(GitBranchesOverlay::new(
                        self.git.branches.clone(),
                        self.focus,
                    )));
                }
            }
            GitEvent::OperationFinished { id, operation: _, message, snapshot } => {
                if !self.git.active_operation.as_ref().is_some_and(|(active, _)| *active == id) {
                    return;
                }
                self.git.active_operation = None;
                self.refresh_git_status_overlay(&snapshot);
                self.git.snapshot = Some(snapshot);
                self.git.load = GitLoadState::Ready;
                self.notification = Some(message);
            }
            GitEvent::Failed { id, operation_applied, failure } => {
                if self.git.active_refresh == Some(id) {
                    let not_repository = failure.kind == GitFailureKind::NotRepository;
                    self.git.load = if self.git.snapshot.is_some() && !not_repository {
                        GitLoadState::Stale(failure.clone())
                    } else {
                        GitLoadState::Unavailable(failure.clone())
                    };
                    if !not_repository {
                        self.notification = Some(format!("Git: {}", failure.message));
                    }
                    self.finish_git_refresh();
                } else if self.git.active_diff == Some(id) {
                    self.git.active_diff = None;
                    if let Some(Overlay::GitDiff(overlay)) = self.overlays.last_mut() {
                        overlay.error = Some(failure.message.clone());
                    }
                    self.notification = Some(format!("Git: {}", failure.message));
                } else if self.git.active_branches == Some(id) {
                    self.git.active_branches = None;
                    self.notification = Some(format!("Git: {}", failure.message));
                } else if self
                    .git
                    .active_operation
                    .as_ref()
                    .is_some_and(|(active, _)| *active == id)
                {
                    self.git.active_operation = None;
                    if operation_applied {
                        self.git.load = if self.git.snapshot.is_some() {
                            GitLoadState::Stale(failure.clone())
                        } else {
                            GitLoadState::Unavailable(failure.clone())
                        };
                    }
                    self.notification = Some(if operation_applied {
                        format!(
                            "Git operation completed, but status refresh failed: {}",
                            failure.message
                        )
                    } else {
                        format!("Git: {}", failure.message)
                    });
                }
            }
        }
    }

    fn finish_git_refresh(&mut self) {
        self.git.active_refresh = None;
        if std::mem::take(&mut self.git.pending_refresh) {
            self.request_git_refresh();
        }
    }

    fn show_git_status(&mut self) {
        // `git.show` is Phase 06's explicit-refresh surface (ADR-0010 §2). The cached
        // snapshot opens immediately; the result re-anchors the overlay when it lands.
        self.request_git_refresh();
        // Re-showing an already-open surface refreshes it in place. Pushing a duplicate
        // would stack overlays *and* lose the selection: the refresh re-anchors whatever
        // is on top, and a fresh overlay starts at row 0.
        if matches!(self.overlays.last(), Some(Overlay::GitStatus(_))) {
            return;
        }
        let Some(snapshot) = self.git.snapshot.as_ref() else {
            self.notification = Some(match &self.git.load {
                GitLoadState::Loading => "Git status is still loading".into(),
                GitLoadState::Unavailable(failure) => {
                    format!("Git status unavailable: {}", failure.message)
                }
                _ => "No Git repository status is available for this workspace".into(),
            });
            return;
        };
        self.overlays.push(Overlay::GitStatus(GitStatusOverlay::new(snapshot, self.focus)));
    }

    pub fn open_selected_git_diff(&mut self) {
        if self.git.active_diff.is_some() {
            self.notification = Some("A Git diff is already loading".into());
            return;
        }
        let selected = match self.overlays.last() {
            Some(Overlay::GitStatus(status)) => status.selected().cloned(),
            _ => None,
        };
        let Some(selected) = selected else {
            self.notification = Some("No changed file is selected".into());
            return;
        };
        let Some(root) = self.explorer.as_ref().map(|explorer| explorer.root.path.clone()) else {
            self.notification = Some("Open a workspace before reviewing Git changes".into());
            return;
        };
        // Git has no recorded version of an untracked path, so `git diff` prints nothing.
        // Say that, rather than rendering an empty diff that reads as "no changes".
        if selected.kind == termesh_core::GitChangeKind::Untracked {
            self.overlays.push(Overlay::GitDiff(GitDiffOverlay::notice(
                selected.path,
                selected.target,
                "Untracked: Git has no recorded version to diff against.\n\
                 Stage it (s) to review it as a diff, or open it in the editor."
                    .into(),
            )));
            return;
        }
        self.next_git_request_id += 1;
        let id = GitRequestId::new(self.next_git_request_id);
        self.git.active_diff = Some(id);
        self.git.diff = None;
        self.git_outbox.push(GitRequest::Diff {
            id,
            root,
            path: selected.path.clone(),
            target: selected.target,
        });
        self.overlays
            .push(Overlay::GitDiff(GitDiffOverlay::loading(selected.path, selected.target)));
    }

    fn selected_git_status_row(&self) -> Option<crate::git_state::GitStatusRow> {
        match self.overlays.last() {
            Some(Overlay::GitStatus(status)) => status.selected().cloned(),
            _ => None,
        }
    }

    fn stage_selected_git_row(&mut self) {
        let Some(row) = self.selected_git_status_row() else {
            self.show_git_status();
            if matches!(self.overlays.last(), Some(Overlay::GitStatus(_))) {
                self.notification = Some("Select a changed file to stage".into());
            }
            return;
        };
        if row.target != termesh_core::GitDiffTarget::Worktree {
            self.notification = Some("Select a worktree change to stage".into());
            return;
        }
        self.queue_git_operation(termesh_core::GitOperation::Stage { path: row.path });
    }

    fn unstage_selected_git_row(&mut self) {
        let Some(row) = self.selected_git_status_row() else {
            self.notification = Some("Open Git Changes and select a staged file".into());
            return;
        };
        if row.target != termesh_core::GitDiffTarget::Index {
            self.notification = Some("Select a staged change to unstage".into());
            return;
        }
        self.queue_git_operation(termesh_core::GitOperation::Unstage { path: row.path });
    }

    fn prompt_git_commit(&mut self) {
        let conflicted = |file: &termesh_core::GitFileStatus| {
            matches!(file.index, Some(termesh_core::GitChangeKind::Conflicted))
                || matches!(file.worktree, Some(termesh_core::GitChangeKind::Conflicted))
        };
        // Git refuses any commit while unmerged paths exist, and an unmerged path also
        // occupies the index — so check conflicts before "is anything staged".
        if self.git.snapshot.as_ref().is_some_and(|snapshot| snapshot.files.iter().any(conflicted))
        {
            self.notification = Some("Resolve and stage the conflicts before committing".into());
            return;
        }
        let staged = self
            .git
            .snapshot
            .as_ref()
            .is_some_and(|snapshot| snapshot.files.iter().any(|file| file.index.is_some()));
        if !staged {
            self.notification = Some("There is nothing staged to commit".into());
            return;
        }
        if self.git.active_operation.is_some() {
            self.notification = Some("A Git operation is already running".into());
            return;
        }
        self.overlays.push(Overlay::Prompt(Prompt {
            title: "Commit staged changes".into(),
            input: String::new(),
            kind: PromptKind::GitCommit,
        }));
    }

    pub fn queue_git_operation(&mut self, operation: termesh_core::GitOperation) {
        if self.git.active_operation.is_some() {
            self.notification = Some("A Git operation is already running".into());
            return;
        }
        let Some(root) = self.explorer.as_ref().map(|explorer| explorer.root.path.clone()) else {
            self.notification = Some("Open a workspace before running Git".into());
            return;
        };
        self.next_git_request_id += 1;
        let id = GitRequestId::new(self.next_git_request_id);
        self.git.active_operation = Some((id, operation.clone()));
        self.git_outbox.push(GitRequest::Execute { id, root, operation });
    }

    fn request_git_branches(&mut self) {
        if self.git.active_branches.is_some() {
            self.notification = Some("Git branches are already loading".into());
            return;
        }
        if self.git.active_operation.is_some() {
            self.notification = Some("A Git operation is already running".into());
            return;
        }
        let Some(root) = self.explorer.as_ref().map(|explorer| explorer.root.path.clone()) else {
            self.notification = Some("Open a workspace before switching branches".into());
            return;
        };
        self.next_git_request_id += 1;
        let id = GitRequestId::new(self.next_git_request_id);
        self.git.active_branches = Some(id);
        self.git_outbox.push(GitRequest::Branches { id, root });
    }

    /// Rebuild an open status overlay against a newer snapshot.
    ///
    /// The selection is re-anchored on the `(group, path)` it was actually pointing at,
    /// not on its index: a background refresh (watcher, agent write) can insert rows
    /// above it, and an index-only clamp would silently slide the cursor onto a different
    /// file — so the next `s` would stage a path the developer never selected.
    fn refresh_git_status_overlay(&mut self, snapshot: &termesh_core::GitRepositorySnapshot) {
        if let Some(Overlay::GitStatus(status)) = self.overlays.last_mut() {
            let previous_focus = status.previous_focus;
            let anchor = status.selected().map(|row| (row.group, row.path.clone()));
            let refreshed = GitStatusOverlay::new(snapshot, previous_focus);
            let selected = anchor
                .and_then(|(group, path)| {
                    refreshed.rows().iter().position(|row| row.group == group && row.path == path)
                })
                // The anchored row is gone (staged, committed, reverted): fall back to the
                // old index, clamped, so the cursor stays near where the developer was.
                .unwrap_or_else(|| status.selected.min(refreshed.rows().len().saturating_sub(1)));
            *status = refreshed;
            status.selected = selected;
        }
    }

    pub fn task_catalog_len(&self) -> usize {
        self.task_catalog.len()
    }

    fn open_task_picker(&mut self) {
        if self.explorer.is_none() {
            self.notification = Some("open a workspace before running a task".into());
        } else if self.task_catalog.is_empty() {
            let kind =
                self.explorer.as_ref().map(|item| item.root.kind.label()).unwrap_or("unknown");
            self.notification = Some(format!("No task adapter for {kind}; use Run in Terminal"));
        } else {
            self.overlays.push(Overlay::Tasks(TaskPicker::new(self.task_catalog.clone())));
        }
    }

    pub fn run_task(&mut self, spec: termesh_core::TaskSpec) {
        let terminal_spec = TerminalSpec {
            program: spec.program.clone(),
            args: spec.args.clone(),
            cwd: spec.cwd.clone(),
            env: Vec::new(),
        };
        let terminal = self.create_terminal(terminal_spec, TerminalOwner::HumanCommand);
        self.attach_task_run(spec, terminal, termesh_core::TaskOrigin::Human);
    }

    fn attach_task_run(
        &mut self,
        spec: termesh_core::TaskSpec,
        terminal: TerminalId,
        origin: termesh_core::TaskOrigin,
    ) {
        let decoder = self.task_service.decoder(&spec);
        if let Some(session) = self.terminals.iter_mut().find(|session| session.id == terminal) {
            session.title = spec.label.clone();
        }
        self.next_task_run_id += 1;
        self.task_runs.push(TaskRun {
            id: termesh_core::TaskRunId::new(self.next_task_run_id),
            spec,
            terminal,
            origin,
            status: termesh_core::TaskStatus::Starting,
            problems: Vec::new(),
            cancel_requested: false,
            decoder,
        });
        if self.task_runs.len() > 20 {
            self.task_runs.remove(0);
        }
    }

    fn cancel_latest_task(&mut self) {
        let Some(run) = self.task_runs.iter_mut().rev().find(|run| {
            matches!(
                run.status,
                termesh_core::TaskStatus::Starting | termesh_core::TaskStatus::Running
            ) && !run.cancel_requested
        }) else {
            self.notification = Some("no running task".into());
            return;
        };
        let Some(session) = self.terminals.iter().find(|session| session.id == run.terminal) else {
            return;
        };
        run.cancel_requested = true;
        self.terminal_outbox
            .push(PtyRequest::Kill { terminal: session.id, generation: session.generation });
    }

    pub fn cancel_search(&mut self) {
        self.search_cancel_outbox = true;
    }

    /// Absorb one correlated search update. Late events from a replaced overlay are
    /// ignored, so an old ripgrep child can never overwrite a newer query.
    pub fn on_search_event(&mut self, event: SearchEvent) {
        let id = match &event {
            SearchEvent::Started { id }
            | SearchEvent::Batch { id, .. }
            | SearchEvent::Finished { id, .. }
            | SearchEvent::Cancelled { id }
            | SearchEvent::Failed { id, .. } => *id,
        };
        let request_preview = {
            let Some(Overlay::Search(search)) = self.overlays.last_mut() else { return };
            if search.request != id {
                return;
            }
            match event {
                SearchEvent::Started { .. } => {
                    if search.mode == SearchMode::Files {
                        search.replace_results(Vec::new());
                    }
                    search.status = SearchStatus::Searching;
                    false
                }
                SearchEvent::Batch { matches, .. } => {
                    search.append_results(matches);
                    search.mode == SearchMode::Text
                }
                SearchEvent::Finished { truncated, .. } => {
                    search.status = SearchStatus::Finished;
                    search.truncated = truncated;
                    false
                }
                SearchEvent::Cancelled { .. } => {
                    search.status = SearchStatus::Cancelled;
                    false
                }
                SearchEvent::Failed { message, .. } => {
                    search.status = SearchStatus::Failed(message);
                    false
                }
            }
        };
        if request_preview {
            self.request_selected_preview();
        }
    }

    fn open_quick_open(&mut self) {
        let Some(root) = self.explorer.as_ref().map(|explorer| explorer.root.path.clone()) else {
            self.notification = Some("open a workspace before using Quick Open".into());
            return;
        };
        self.next_search_request_id += 1;
        let id = SearchRequestId::new(self.next_search_request_id);
        self.overlays.push(Overlay::Search(SearchOverlay::files(id, root.clone(), self.focus)));
        self.search_outbox.push(SearchRequest {
            id,
            root,
            mode: SearchMode::Files,
            query: String::new(),
            limit: 20_000,
        });
    }

    fn open_workspace_search(&mut self) {
        let Some(root) = self.explorer.as_ref().map(|explorer| explorer.root.path.clone()) else {
            self.notification = Some("open a workspace before searching".into());
            return;
        };
        self.next_search_request_id += 1;
        let id = SearchRequestId::new(self.next_search_request_id);
        self.overlays.push(Overlay::Search(SearchOverlay::text(id, root, self.focus)));
    }

    pub fn search_query_changed(&mut self) {
        let Some(Overlay::Search(search)) = self.overlays.last() else { return };
        if search.mode != SearchMode::Text {
            return;
        }
        let query = search.query.clone();
        let root = search.root.clone();
        self.next_search_request_id += 1;
        let id = SearchRequestId::new(self.next_search_request_id);

        // Every open buffer is scanned, not just the dirty ones, and it happens here on
        // the event loop on each keystroke. Both parts are deliberate. A clean buffer's
        // file may sit outside the workspace root or inside an ignore rule, where the
        // worker's walk will never reach it, so skipping clean buffers would silently
        // drop results rather than defer them. And the overlay is the *immediate*
        // feedback half of search — the 75ms debounce lives in the worker precisely so
        // this half can stay instant. The cost is bounded by open-buffer bytes, which is
        // the same order as a re-highlight; if that ever shows up in practice, the fix
        // is an incremental matcher, not a narrower set of buffers (ADR-0009 §1).
        let mut live_matches = Vec::new();
        let mut live_paths = HashSet::new();
        if !query.is_empty() {
            for buffer in &self.buffers {
                let Some(path) = buffer.path() else { continue };
                live_paths.insert(path.to_path_buf());
                live_matches.extend(termesh_search::literal_matches(
                    path,
                    &buffer.text().to_string(),
                    &query,
                ));
            }
        }
        let Some(Overlay::Search(search)) = self.overlays.last_mut() else { return };
        search.set_request(id);
        search.status = SearchStatus::Waiting;
        search.truncated = false;
        search.set_live_results(live_matches, live_paths);
        if query.is_empty() {
            self.cancel_search();
            return;
        }
        self.search_outbox.push(SearchRequest {
            id,
            root,
            mode: SearchMode::Text,
            query,
            limit: 1_000,
        });
        self.request_selected_preview();
    }

    pub fn request_selected_preview(&mut self) -> Option<PreviewRequestId> {
        let selected = match self.overlays.last() {
            Some(Overlay::Search(search)) if search.mode == SearchMode::Text => {
                search.selected().cloned()
            }
            _ => None,
        }?;
        let line = selected.line.unwrap_or(1);
        let already_current = match self.overlays.last() {
            Some(Overlay::Search(search)) => search
                .preview_key()
                .is_some_and(|(path, preview_line)| path == selected.path && preview_line == line),
            _ => false,
        };
        if already_current {
            return self.overlays.last().and_then(|overlay| match overlay {
                Overlay::Search(search) => search.preview_request(),
                _ => None,
            });
        }

        if let Some(buffer) =
            self.buffers.iter().find(|buffer| buffer.path() == Some(&selected.path))
        {
            let (start_line, text) = preview_window(&buffer.text().to_string(), line, 10);
            if let Some(Overlay::Search(search)) = self.overlays.last_mut() {
                search.set_preview(selected.path, line, start_line, text);
            }
            return None;
        }

        self.next_preview_request_id += 1;
        let request = PreviewRequestId::new(self.next_preview_request_id);
        if let Some(Overlay::Search(search)) = self.overlays.last_mut() {
            search.await_preview(request, selected.path.clone(), line);
        }
        self.outbox.push(FsRequest::ReadPreview {
            request,
            path: selected.path,
            line,
            context: 10,
        });
        Some(request)
    }

    /// Drain queued agent work. Same shape as [`Self::take_fs_requests`], so the model
    /// stays a pure state machine that *emits* requests rather than performing them.
    pub fn take_agent_requests(&mut self) -> Vec<AgentRequest> {
        std::mem::take(&mut self.agent_outbox)
    }

    /// Paths currently recorded as served to the agent from a live buffer. Test-only: it
    /// exists to prove an unopened read does *not* claim a buffer version it never took
    /// (ADR-0014 Task 1), not as a surface any production code should consult.
    #[cfg(test)]
    pub fn served_reads_for_test(&self) -> Vec<&Path> {
        self.served_reads.iter().map(|r| r.path.as_path()).collect()
    }

    /// The active terminal tab, if any.
    pub fn active_terminal(&self) -> Option<&TerminalSession> {
        self.active_terminal.and_then(|index| self.terminals.get(index))
    }

    /// Drain PTY effects for the worker. PTY handles never enter model state.
    pub fn take_pty_requests(&mut self) -> Vec<PtyRequest> {
        std::mem::take(&mut self.terminal_outbox)
    }

    pub fn set_permission_policy(&mut self, policy: PermissionPolicy) {
        self.permission_policy = policy;
    }

    pub fn permission_policy(&self) -> &PermissionPolicy {
        &self.permission_policy
    }

    pub fn terminal_copy_mode(&self) -> bool {
        self.terminal_copy_mode
    }

    /// Whether the focused terminal has a process which can consume shell input.
    /// Exited, failed, released, and empty terminal panes fall back to global bindings.
    pub fn terminal_accepts_input(&self) -> bool {
        self.active_terminal().is_some_and(|session| {
            !session.released
                && matches!(
                    session.status,
                    TerminalStatus::Starting | TerminalStatus::Running { .. }
                )
        })
    }

    pub fn active_terminal_has_running_task(&self) -> bool {
        let Some(terminal) = self.active_terminal().map(|session| session.id) else { return false };
        self.task_runs.iter().any(|run| {
            run.terminal == terminal
                && matches!(
                    run.status,
                    termesh_core::TaskStatus::Starting | termesh_core::TaskStatus::Running
                )
        })
    }

    /// Drain human-authored copy effects for the runtime clipboard service.
    pub fn take_clipboard_text(&mut self) -> Vec<String> {
        std::mem::take(&mut self.clipboard_outbox)
    }

    /// Encode one chord for the active PTY. Normal terminal focus never reaches the
    /// global keymap, so Ctrl+C and Tab remain shell input.
    pub fn type_terminal_chord(&mut self, chord: termesh_core::input::KeyChord) {
        let Some(session) = self.active_terminal() else { return };
        if session.released
            || !matches!(session.status, TerminalStatus::Starting | TerminalStatus::Running { .. })
        {
            return;
        }
        let terminal = session.id;
        let generation = session.generation;
        let modes = session.screen.input_modes();
        if let Some(bytes) = termesh_terminal::encode_key(chord, modes) {
            self.terminal_outbox.push(PtyRequest::Write { terminal, generation, bytes });
        }
    }

    /// Resize retained grids and every process which can still receive output.
    pub fn set_terminal_size(&mut self, size: TerminalSize) {
        let size = TerminalSize { rows: size.rows.max(1), cols: size.cols.max(1) };
        if size == self.terminal_size {
            return;
        }
        self.terminal_size = size;
        for terminal in &mut self.terminals {
            terminal.screen.resize(size);
            if !terminal.released
                && matches!(
                    terminal.status,
                    TerminalStatus::Starting | TerminalStatus::Running { .. }
                )
            {
                self.terminal_outbox.push(PtyRequest::Resize {
                    terminal: terminal.id,
                    generation: terminal.generation,
                    size,
                });
            }
        }
    }

    /// Absorb one event from the PTY worker into only its named terminal.
    pub fn on_pty_event(&mut self, event: PtyEvent) {
        let (terminal, generation) = match &event {
            PtyEvent::Spawned { terminal, generation, .. }
            | PtyEvent::Output { terminal, generation, .. }
            | PtyEvent::Exited { terminal, generation, .. }
            | PtyEvent::Failed { terminal, generation, .. } => (*terminal, *generation),
        };
        let Some(session) = self.terminals.iter().find(|item| item.id == terminal) else {
            return;
        };
        if session.generation != generation || session.released {
            return;
        }
        match event {
            PtyEvent::Spawned { terminal, generation, process_id } => {
                if let Some(session) = self.terminals.iter_mut().find(|item| item.id == terminal) {
                    if !session.released {
                        session.status = TerminalStatus::Running { process_id };
                    }
                }
                if let Some(run) = self.task_runs.iter_mut().find(|run| run.terminal == terminal) {
                    run.status = termesh_core::TaskStatus::Running;
                }
                if let Some(index) = self.pending_terminal_creates.iter().position(|pending| {
                    pending.terminal == terminal && pending.generation == generation
                }) {
                    let pending = self.pending_terminal_creates.remove(index);
                    self.respond_terminal(
                        pending.request,
                        AgentTerminalResponse::Created { terminal },
                    );
                }
            }
            PtyEvent::Output { terminal, generation, bytes } => {
                let bytes = self.decode_task_output(terminal, Some(&bytes));
                let Some(session) = self.terminals.iter_mut().find(|item| item.id == terminal)
                else {
                    return;
                };
                if !matches!(
                    session.status,
                    TerminalStatus::Starting | TerminalStatus::Running { .. }
                ) {
                    return;
                }
                session.screen.feed(&bytes);
                session.capture.push(&bytes);
                if let Some(title) = session.screen.title().filter(|title| !title.is_empty()) {
                    session.title = title;
                }
                for bytes in session.screen.take_responses() {
                    self.terminal_outbox.push(PtyRequest::Write { terminal, generation, bytes });
                }
            }
            PtyEvent::Exited { terminal, exit, .. } => {
                let trailing = self.decode_task_output(terminal, None);
                if let Some(session) = self.terminals.iter_mut().find(|item| item.id == terminal) {
                    session.screen.feed(&trailing);
                    session.capture.push(&trailing);
                    session.capture.finish();
                    session.status = TerminalStatus::Exited(exit.clone());
                }
                if let Some(run) = self.task_runs.iter_mut().find(|run| run.terminal == terminal) {
                    run.status = if run.cancel_requested {
                        termesh_core::TaskStatus::Cancelled
                    } else if exit.code == Some(0) {
                        termesh_core::TaskStatus::Succeeded
                    } else {
                        termesh_core::TaskStatus::Failed
                    };
                }
                self.complete_terminal_waits(terminal, AgentTerminalResponse::Exited(exit));
                self.request_git_refresh();
            }
            PtyEvent::Failed { terminal, generation, message } => {
                // A terminal that already reported how it ended keeps that answer. The
                // PTY stays open until `Release`, so a write racing the child's exit
                // fails EIO *after* the exit is recorded; letting that overwrite the
                // status would report a clean run as a failure and hand the agent a null
                // exit code for a process that exited 0.
                let already_ended = self
                    .terminals
                    .iter()
                    .find(|item| item.id == terminal)
                    .is_some_and(|item| matches!(item.status, TerminalStatus::Exited(_)));
                if already_ended {
                    return;
                }
                if let Some(session) = self.terminals.iter_mut().find(|item| item.id == terminal) {
                    session.capture.finish();
                    session.status = TerminalStatus::Failed(message.clone());
                    self.notification = Some(format!("terminal: {message}"));
                }
                if let Some(run) = self.task_runs.iter_mut().find(|run| run.terminal == terminal) {
                    run.status = termesh_core::TaskStatus::Failed;
                }
                if let Some(index) = self.pending_terminal_creates.iter().position(|pending| {
                    pending.terminal == terminal && pending.generation == generation
                }) {
                    let pending = self.pending_terminal_creates.remove(index);
                    self.respond_terminal(
                        pending.request,
                        AgentTerminalResponse::Error(message.clone()),
                    );
                }
                self.complete_terminal_waits(terminal, AgentTerminalResponse::Error(message));
                self.request_git_refresh();
            }
        }
    }

    fn decode_task_output(&mut self, terminal: TerminalId, bytes: Option<&[u8]>) -> Vec<u8> {
        let Some(run) = self.task_runs.iter_mut().find(|run| run.terminal == terminal) else {
            return bytes.unwrap_or_default().to_vec();
        };
        let Some(decoder) = run.decoder.as_mut() else {
            return bytes.unwrap_or_default().to_vec();
        };
        let decoded = match bytes {
            Some(bytes) => decoder.push(bytes),
            None => decoder.finish(),
        };
        let was_empty = run.problems.is_empty();
        for problem in decoded.problems {
            let Some(problem) = normalize_problem(&run.spec.cwd, problem) else { continue };
            let duplicate = run.problems.iter().any(|existing| {
                existing.path == problem.path
                    && existing.line == problem.line
                    && existing.column == problem.column
                    && existing.message == problem.message
            });
            if !duplicate && run.problems.len() < 500 {
                run.problems.push(problem);
            }
        }
        // This run has just taken over as the F8 list (see `problem_rows`); a cursor
        // left over from the previous run indexes a list that is no longer showing.
        if was_empty && !run.problems.is_empty() {
            self.problem_cursor = None;
        }
        decoded.display
    }

    /// Queue releases for every retained PTY before the runtime drops its worker.
    pub fn shutdown_terminals(&mut self) {
        self.reject_pending_permission(None, "application shutting down");
        let creates = std::mem::take(&mut self.pending_terminal_creates);
        for pending in creates {
            self.respond_terminal(
                pending.request,
                AgentTerminalResponse::Error("application shutting down".into()),
            );
        }
        let waits = std::mem::take(&mut self.pending_terminal_waits);
        for pending in waits {
            self.respond_terminal(
                pending.request,
                AgentTerminalResponse::Error("application shutting down".into()),
            );
        }
        for terminal in &mut self.terminals {
            if terminal.released {
                continue;
            }
            if matches!(terminal.status, TerminalStatus::Starting | TerminalStatus::Running { .. })
            {
                self.terminal_outbox.push(PtyRequest::Kill {
                    terminal: terminal.id,
                    generation: terminal.generation,
                });
            }
            self.terminal_outbox.push(PtyRequest::Release {
                terminal: terminal.id,
                generation: terminal.generation,
            });
            terminal.released = true;
        }
    }

    fn terminal_cwd(&self) -> PathBuf {
        self.explorer
            .as_ref()
            .map(|explorer| explorer.root.path.clone())
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."))
    }

    fn create_terminal(&mut self, spec: TerminalSpec, owner: TerminalOwner) -> TerminalId {
        self.create_terminal_with_limit(spec, owner, DEFAULT_CAPTURE_LIMIT)
    }

    fn create_terminal_with_limit(
        &mut self,
        spec: TerminalSpec,
        owner: TerminalOwner,
        output_byte_limit: usize,
    ) -> TerminalId {
        self.next_terminal_id += 1;
        let id = TerminalId::new(self.next_terminal_id);
        let agent_owned = matches!(owner, TerminalOwner::Agent { .. });
        self.terminals.push(TerminalSession {
            id,
            generation: TerminalGeneration::new(1),
            spec: spec.clone(),
            owner,
            title: format!("Terminal {}", id.0),
            status: TerminalStatus::Starting,
            screen: TerminalScreen::new(self.terminal_size),
            capture: CapturedOutput::new(output_byte_limit),
            released: false,
        });
        // Focus follows the user, never the agent. A command spawning while they type
        // would otherwise redirect the rest of their keystrokes into its stdin — terminal
        // focus is shell-first (ADR-0008 §3), so nothing downstream would catch it. For
        // the same reason an agent terminal only becomes the visible tab when the user is
        // not already looking at another one.
        let index = self.terminals.len() - 1;
        if agent_owned {
            if self.focus != Pane::Terminal {
                self.active_terminal = Some(index);
            }
        } else {
            self.active_terminal = Some(index);
            self.focus_terminal_pane();
        }
        self.terminal_outbox.push(PtyRequest::Spawn {
            terminal: id,
            generation: TerminalGeneration::new(1),
            spec,
            size: self.terminal_size,
        });
        id
    }

    fn focus_terminal_pane(&mut self) {
        if self.focus != Pane::Terminal {
            self.previous_non_terminal_focus = self.focus;
        }
        self.focus = Pane::Terminal;
    }

    /// Focus one named pane directly. Unlike the Tab ring this always works from inside
    /// a terminal, which is the point: cycling cannot carry you out of a pane that owns
    /// the cycle key.
    fn focus_pane(&mut self, pane: Pane) {
        if self.focus == Pane::Terminal && pane != Pane::Terminal {
            self.cancel_terminal_copy_mode();
        }
        self.focus = pane;
    }

    fn toggle_terminal_focus(&mut self) {
        if self.focus == Pane::Terminal {
            self.cancel_terminal_copy_mode();
            self.focus = self.previous_non_terminal_focus;
            return;
        }
        if self.terminals.is_empty() {
            let spec =
                termesh_platform::shell(self.settings.shell.as_deref(), &self.terminal_cwd());
            self.create_terminal(spec, TerminalOwner::HumanShell);
        } else {
            self.focus_terminal_pane();
        }
    }

    fn new_shell_terminal(&mut self) {
        let spec = termesh_platform::shell(self.settings.shell.as_deref(), &self.terminal_cwd());
        self.create_terminal(spec, TerminalOwner::HumanShell);
    }

    fn cycle_terminal(&mut self, delta: isize) {
        if self.terminals.is_empty() {
            self.new_shell_terminal();
            return;
        }
        let current = self.active_terminal.unwrap_or(0) as isize;
        let len = self.terminals.len() as isize;
        self.active_terminal = Some((current + delta).rem_euclid(len) as usize);
        self.focus_terminal_pane();
    }

    fn restart_terminal(&mut self) {
        self.cancel_terminal_copy_mode();
        let Some(index) = self.active_terminal else {
            self.new_shell_terminal();
            return;
        };
        if self
            .pending_terminal_creates
            .iter()
            .any(|pending| pending.terminal == self.terminals[index].id)
        {
            self.notification = Some("agent terminal is still starting".into());
            return;
        }
        let terminal = self.terminals[index].id;
        self.complete_terminal_waits(
            terminal,
            AgentTerminalResponse::Error("terminal restarted".into()),
        );
        let session = &mut self.terminals[index];
        if !session.released {
            if matches!(session.status, TerminalStatus::Starting | TerminalStatus::Running { .. }) {
                self.terminal_outbox.push(PtyRequest::Kill {
                    terminal: session.id,
                    generation: session.generation,
                });
            }
            self.terminal_outbox
                .push(PtyRequest::Release { terminal: session.id, generation: session.generation });
        }
        session.generation = TerminalGeneration::new(session.generation.0 + 1);
        session.screen = TerminalScreen::new(self.terminal_size);
        session.capture = CapturedOutput::new(DEFAULT_CAPTURE_LIMIT);
        session.status = TerminalStatus::Starting;
        session.released = false;
        self.terminal_outbox.push(PtyRequest::Spawn {
            terminal: session.id,
            generation: session.generation,
            spec: session.spec.clone(),
            size: self.terminal_size,
        });
    }

    /// The problem list F8 walks: the newest run that actually *found* something.
    ///
    /// Not simply the newest run. Starting a fresh build — or an agent starting any task
    /// of its own — would otherwise empty the list out from under a human who is still
    /// working through the failures of the run before it. Results survive until another
    /// run replaces them with results of its own.
    fn latest_problem_run(&self) -> Option<&TaskRun> {
        self.task_runs.iter().rev().find(|run| !run.problems.is_empty())
    }

    /// The merged, human-facing Problems rows. Coordinates are one-based before the
    /// dedup key is formed, so Cargo and LSP reports for one rustc diagnostic coincide.
    pub fn problem_rows(&self) -> Vec<ProblemRow> {
        let mut rows: BTreeMap<(PathBuf, usize, String), ProblemRow> = BTreeMap::new();

        if let Some(run) = self.latest_problem_run() {
            for task_problem in &run.problems {
                let Some(problem) = normalize_problem(&run.spec.cwd, task_problem.clone()) else {
                    continue;
                };
                let message_key = normalize_problem_message(&problem.message);
                rows.insert(
                    (problem.path.clone(), problem.line, message_key),
                    ProblemRow {
                        path: problem.path,
                        line: problem.line,
                        column: problem.column,
                        severity: match problem.severity {
                            termesh_core::ProblemSeverity::Error => {
                                termesh_core::DiagnosticSeverity::Error
                            }
                            termesh_core::ProblemSeverity::Warning => {
                                termesh_core::DiagnosticSeverity::Warning
                            }
                        },
                        origin: termesh_core::DiagnosticOrigin::Task,
                        source: executable_name(&run.spec.program),
                        message: problem.message,
                    },
                );
            }
        }

        let workspace = self.explorer.as_ref().map(|explorer| explorer.root.path.as_path());
        for diagnostics in self.lsp.diagnostics.values() {
            for diagnostic in diagnostics {
                let path = if diagnostic.path.is_relative() {
                    let Some(root) = workspace else { continue };
                    root.join(&diagnostic.path)
                } else {
                    diagnostic.path.clone()
                };
                if path.components().any(|component| component == std::path::Component::ParentDir) {
                    continue;
                }
                let line = diagnostic.range.start.line as usize + 1;
                let message_key = normalize_problem_message(&diagnostic.message);
                // Inserted second on purpose: live server diagnostics replace duplicate
                // task rows while task-only build errors remain visible.
                rows.insert(
                    (path.clone(), line, message_key),
                    ProblemRow {
                        path,
                        line,
                        column: diagnostic.range.start.character as usize + 1,
                        severity: diagnostic.severity,
                        origin: termesh_core::DiagnosticOrigin::LanguageServer,
                        source: if diagnostic.source.is_empty() {
                            "language-server".into()
                        } else {
                            diagnostic.source.clone()
                        },
                        message: diagnostic.message.clone(),
                    },
                );
            }
        }

        rows.into_values().collect()
    }

    fn show_problems(&mut self) {
        let problems = self.problem_rows();
        if problems.is_empty() {
            self.notification = Some("No problems".into());
            return;
        }
        let selected = self.problem_cursor.unwrap_or(0).min(problems.len() - 1);
        self.overlays.push(Overlay::Problems(ProblemsOverlay::new(problems, selected)));
    }

    fn step_problem(&mut self, forward: bool) {
        let problems = self.problem_rows();
        if problems.is_empty() {
            self.notification = Some("No problems".into());
            return;
        }
        let current =
            self.problem_cursor.unwrap_or_else(|| if forward { problems.len() - 1 } else { 0 });
        let next = if forward {
            (current + 1) % problems.len()
        } else {
            (current + problems.len() - 1) % problems.len()
        };
        self.problem_cursor = Some(next);
        self.navigate_problem(problems[next].navigation_problem());
    }

    pub fn navigate_problem(&mut self, problem: termesh_core::Problem) {
        if let Some(index) = self
            .problem_rows()
            .iter()
            .position(|candidate| candidate.navigation_problem() == problem)
        {
            self.problem_cursor = Some(index);
        }
        self.next_location_request_id += 1;
        let request = LocationRequestId::new(self.next_location_request_id);
        self.pending_problem_navigation =
            Some(PendingProblemNavigation { request, problem: problem.clone() });
        self.outbox.push(FsRequest::ResolvePath { request, path: problem.path });
    }

    fn close_terminal(&mut self) {
        let Some(session) = self.active_terminal() else {
            self.notification = Some("no terminal to close".into());
            return;
        };
        if !session.released
            && matches!(session.status, TerminalStatus::Starting | TerminalStatus::Running { .. })
        {
            self.overlays.push(Overlay::Prompt(Prompt {
                title: format!("Close {} and stop its process?  (Enter to confirm)", session.title),
                input: String::new(),
                kind: PromptKind::ConfirmCloseTerminal { terminal: session.id },
            }));
            return;
        }
        self.remove_terminal(session.id, false);
    }

    fn remove_terminal(&mut self, terminal: TerminalId, kill: bool) {
        let Some(index) = self.terminals.iter().position(|session| session.id == terminal) else {
            return;
        };
        if let Some(create_index) =
            self.pending_terminal_creates.iter().position(|pending| pending.terminal == terminal)
        {
            let pending = self.pending_terminal_creates.remove(create_index);
            self.respond_terminal(
                pending.request,
                AgentTerminalResponse::Error("terminal closed".into()),
            );
        }
        self.complete_terminal_waits(
            terminal,
            AgentTerminalResponse::Error("terminal closed".into()),
        );
        let previous_active = self.active_terminal;
        let session = self.terminals.remove(index);
        if previous_active == Some(index) {
            self.terminal_copy_mode = false;
        }
        if !session.released {
            if kill {
                self.terminal_outbox
                    .push(PtyRequest::Kill { terminal, generation: session.generation });
            }
            self.terminal_outbox
                .push(PtyRequest::Release { terminal, generation: session.generation });
        }
        self.active_terminal = if self.terminals.is_empty() {
            if self.focus == Pane::Terminal {
                self.focus = self.previous_non_terminal_focus;
            }
            None
        } else {
            // Track the tab the user was on, not the one that was closed — closing any
            // *other* tab must leave the selection where it is. Agent events can close
            // or open tabs while a close prompt is up, so the two are not always equal.
            let last = self.terminals.len() - 1;
            Some(match previous_active {
                Some(active) if active == index => index.min(last), // it was the active one
                Some(active) if active > index => active - 1,       // everything shifted down
                Some(active) => active,                             // removed after it: unchanged
                None => index.min(last),
            })
        };
    }

    fn enter_terminal_copy_mode(&mut self) {
        if self.active_terminal().is_none() {
            self.notification = Some("no terminal to copy from".into());
            return;
        }
        self.focus_terminal_pane();
        self.terminal_copy_mode = true;
        if let Some(index) = self.active_terminal {
            self.terminals[index].screen.begin_selection();
        }
    }

    fn move_terminal_copy_selection(&mut self, rows: i32, cols: i32, extend: bool) {
        if !self.terminal_copy_mode {
            return;
        }
        if let Some(index) = self.active_terminal {
            self.terminals[index].screen.move_selection(rows, cols, extend);
        }
    }

    fn page_terminal_copy_selection(&mut self, direction: i32) {
        let rows = i32::from(self.terminal_size.rows).saturating_mul(direction);
        self.move_terminal_copy_selection(rows, 0, false);
    }

    /// Move the terminal viewport by a page. Positive scrolls back into history.
    ///
    /// One line of overlap so the reader keeps their place across a page, matching how
    /// pagers behave. Clamping is alacritty's: scrolling past either end is a no-op
    /// rather than an error, so holding the key at the top of the buffer is harmless.
    fn scroll_terminal_page(&mut self, direction: i32) {
        let page = i32::from(self.terminal_size.rows).saturating_sub(1).max(1);
        if let Some(index) = self.active_terminal {
            self.terminals[index].screen.scroll_lines(page.saturating_mul(direction));
        }
    }

    fn confirm_terminal_copy_mode(&mut self) {
        if !self.terminal_copy_mode {
            return;
        }
        if let Some(index) = self.active_terminal {
            let session = &mut self.terminals[index];
            if let Some(text) = session.screen.selected_text().filter(|text| !text.is_empty()) {
                self.clipboard_outbox.push(text);
            }
            session.screen.clear_selection();
        }
        self.terminal_copy_mode = false;
    }

    fn cancel_terminal_copy_mode(&mut self) {
        if let Some(index) = self.active_terminal {
            self.terminals[index].screen.clear_selection();
        }
        self.terminal_copy_mode = false;
    }

    // --- the agent ----------------------------------------------------------------

    /// Absorb something the agent produced.
    ///
    /// The single entry point for agent state, reached identically by the scripted agent
    /// in tests and by the real ACP worker in the running app (ADR-0007 §2). A fake that
    /// took a different route would be testing a path the product never uses.
    pub fn on_agent_event(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::Ready { capabilities } => {
                self.agent_capabilities = Some(capabilities);
            }
            AgentEvent::ModesAvailable { session, current, available } => {
                if let Some(agent) = self.agent.as_mut().filter(|agent| agent.id == session) {
                    agent.modes = available;
                    agent.current_mode = Some(current);
                }
            }
            AgentEvent::ModeChanged { session, mode } => {
                if let Some(agent) = self.agent.as_mut().filter(|agent| agent.id == session) {
                    let name = agent
                        .modes
                        .iter()
                        .find(|candidate| candidate.id == mode)
                        .map(|candidate| candidate.name.clone())
                        .unwrap_or_else(|| mode.clone());
                    agent.current_mode = Some(mode);
                    self.notification = Some(format!("agent mode: {name}"));
                }
            }
            AgentEvent::SessionStarted { session } => {
                if let Some(previous) = self.agent.as_ref().map(|agent| agent.id) {
                    self.cancel_agent_terminals(previous, "agent session replaced");
                }
                self.agent = Some(AgentSession {
                    id: session,
                    transcript: Vec::new(),
                    proposals: Vec::new(),
                    modes: Vec::new(),
                    current_mode: None,
                    pending_permission: None,
                    attached_terminals: Vec::new(),
                    turn_active: false,
                });
                if let Some(text) = self.pending_prompt.take() {
                    self.send_prompt(session, text);
                }
            }
            AgentEvent::MessageChunk { text, .. } => self.push_transcript(Speaker::Agent, &text),
            // Reasoning is kept but marked, so the pane can render it dimmer than the
            // answer rather than passing it off as one.
            AgentEvent::ThoughtChunk { text, .. } => self.push_transcript(Speaker::Thought, &text),
            AgentEvent::ReadFileRequested { session, request, path } => {
                self.serve_read(session, request, path)
            }
            AgentEvent::ProposedEdit { proposal, path, old_text, new_text, .. } => {
                self.receive_proposal(proposal, path, old_text, new_text);
            }
            AgentEvent::PermissionRequested {
                session,
                request,
                summary,
                command,
                terminal_spec,
                edit,
            } => {
                // Every request leaves here with an answer. There is one prompt slot, so
                // a request we cannot show has to be rejected on the spot — overwriting
                // the slot dropped the previous origin, and a `terminal/create` waiting
                // on it would never be replied to, hanging the agent for the session.
                let busy = self
                    .agent
                    .as_ref()
                    .filter(|agent| agent.id == session)
                    .map(|agent| agent.pending_permission.is_some());
                match busy {
                    Some(false) => {
                        // An agent that stops to ask before editing is one whose edit can be
                        // reviewed, so the diff is prepared before the prompt goes up. When it
                        // cannot be placed, `summary` says why and the prompt stays a prompt —
                        // an approval with no diff is worse than useless, but a *silent* one is
                        // worse still (ADR-0016 §1a).
                        let (review, summary) = match edit {
                            Some(edit) => self.review_for_permission(request, edit, summary),
                            None => (None, summary),
                        };
                        if let Some(agent) = self.agent.as_mut() {
                            agent.pending_permission = Some(PendingPermission {
                                origin: PermissionOrigin::AgentRequest { request, terminal_spec },
                                summary,
                                command,
                                review,
                            });
                        }
                    }
                    // Busy, or for a session we no longer have (an unresolved id, or one
                    // already replaced by `SessionStarted`). Either way: reject, do not
                    // drop — ACP has no timeout to rescue a silent request.
                    _ => self.agent_outbox.push(AgentRequest::Permission {
                        request,
                        decision: termesh_core::PermissionDecision::RejectOnce,
                    }),
                }
            }
            AgentEvent::TerminalRequest { session, request, operation } => {
                self.handle_agent_terminal(session, request, operation)
            }
            AgentEvent::TerminalAttached { session, terminal } => {
                let owned = self.terminals.iter().any(|candidate| {
                    candidate.id == terminal
                        && candidate.owner == (TerminalOwner::Agent { session })
                });
                if owned {
                    if let Some(agent) = self.agent.as_mut().filter(|agent| agent.id == session) {
                        if !agent.attached_terminals.contains(&terminal) {
                            agent.attached_terminals.push(terminal);
                        }
                    }
                }
            }
            AgentEvent::TurnEnded { session, reason } => {
                if let Some(agent) = self.agent.as_mut() {
                    agent.turn_active = false;
                }
                if reason == StopReason::Cancelled {
                    self.cancel_agent_terminals(session, "agent session cancelled");
                }
            }
            AgentEvent::Failed { session, message } => {
                if let Some(agent) = self.agent.as_mut() {
                    agent.turn_active = false;
                }
                self.cancel_agent_terminals(session, "agent session failed");
                self.notification = Some(format!("agent: {message}"));
            }
        }
    }

    /// Start a session, telling the agent the workspace root as its cwd (ADR-0007 §4).
    fn new_agent_session(&mut self) {
        let cwd = match &self.explorer {
            Some(e) => e.root.path.clone(),
            None => {
                self.notification = Some("open a workspace before starting an agent".into());
                return;
            }
        };
        self.agent_outbox.push(AgentRequest::NewSession { cwd });
    }

    fn handle_agent_terminal(
        &mut self,
        session: SessionId,
        request: AgentTerminalRequestId,
        operation: AgentTerminalOperation,
    ) {
        if self.agent.as_ref().is_none_or(|agent| agent.id != session) {
            self.respond_terminal(
                request,
                AgentTerminalResponse::Error("unknown agent session".into()),
            );
            return;
        }

        if let AgentTerminalOperation::Create { spec, output_byte_limit, preauthorized } = operation
        {
            let permitted = preauthorized
                || self.explorer.as_ref().is_some_and(|explorer| {
                    self.permission_policy.permits(&explorer.root.path, &spec)
                });
            if permitted {
                self.spawn_agent_terminal(session, request, spec, output_byte_limit);
                return;
            }
            let Some(agent) = self.agent.as_mut() else { return };
            if agent.pending_permission.is_some() {
                self.respond_terminal(
                    request,
                    AgentTerminalResponse::Error("another permission request is pending".into()),
                );
                return;
            }
            let mut command = vec![spec.program.clone()];
            command.extend(spec.args.clone());
            agent.pending_permission = Some(PendingPermission {
                summary: format!("Run {} in a managed terminal?", spec.program),
                command,
                origin: PermissionOrigin::TerminalCreate {
                    session,
                    request,
                    spec,
                    output_byte_limit,
                },
                // Running a command is not an edit; there is nothing to diff.
                review: None,
            });
            return;
        }

        let operation_terminal = match &operation {
            AgentTerminalOperation::Create { .. } => unreachable!(),
            AgentTerminalOperation::Output { terminal }
            | AgentTerminalOperation::WaitForExit { terminal }
            | AgentTerminalOperation::Kill { terminal }
            | AgentTerminalOperation::Release { terminal } => *terminal,
        };
        let Some(index) = self.terminals.iter().position(|terminal| {
            terminal.id == operation_terminal
                && terminal.owner == (TerminalOwner::Agent { session })
                && !terminal.released
        }) else {
            self.respond_terminal(
                request,
                AgentTerminalResponse::Error("unknown or released terminal".into()),
            );
            return;
        };
        let terminal = self.terminals[index].id;
        match operation {
            AgentTerminalOperation::Create { .. } => unreachable!(),
            AgentTerminalOperation::Output { .. } => {
                let session = &self.terminals[index];
                let exit = match &session.status {
                    TerminalStatus::Exited(exit) => Some(exit.clone()),
                    _ => None,
                };
                self.respond_terminal(
                    request,
                    AgentTerminalResponse::Output {
                        output: session.capture.as_str().to_string(),
                        truncated: session.capture.truncated(),
                        exit,
                    },
                );
            }
            AgentTerminalOperation::WaitForExit { .. } => {
                let response = match &self.terminals[index].status {
                    TerminalStatus::Exited(exit) => {
                        Some(AgentTerminalResponse::Exited(exit.clone()))
                    }
                    TerminalStatus::Failed(message) => {
                        Some(AgentTerminalResponse::Error(message.clone()))
                    }
                    _ => None,
                };
                if let Some(response) = response {
                    self.respond_terminal(request, response);
                } else {
                    self.pending_terminal_waits.push(PendingTerminalWait { request, terminal });
                }
            }
            AgentTerminalOperation::Kill { .. } => {
                let generation = self.terminals[index].generation;
                self.terminal_outbox.push(PtyRequest::Kill { terminal, generation });
                self.complete_terminal_waits(
                    terminal,
                    AgentTerminalResponse::Exited(termesh_core::TerminalExit {
                        code: None,
                        signal: Some("killed".into()),
                    }),
                );
                self.respond_terminal(request, AgentTerminalResponse::Acknowledged);
            }
            AgentTerminalOperation::Release { .. } => {
                if matches!(
                    self.terminals[index].status,
                    TerminalStatus::Starting | TerminalStatus::Running { .. }
                ) {
                    self.terminal_outbox.push(PtyRequest::Kill {
                        terminal,
                        generation: self.terminals[index].generation,
                    });
                }
                self.terminal_outbox.push(PtyRequest::Release {
                    terminal,
                    generation: self.terminals[index].generation,
                });
                self.terminals[index].released = true;
                self.complete_terminal_waits(
                    terminal,
                    AgentTerminalResponse::Error("terminal released".into()),
                );
                self.respond_terminal(request, AgentTerminalResponse::Acknowledged);
            }
        }
    }

    fn spawn_agent_terminal(
        &mut self,
        session: SessionId,
        request: AgentTerminalRequestId,
        spec: TerminalSpec,
        output_byte_limit: usize,
    ) {
        let task = self
            .task_catalog
            .iter()
            .find(|task| {
                task.program == spec.program
                    && task.args == spec.args
                    && task.cwd == spec.cwd
                    && spec.env.is_empty()
            })
            .cloned();
        let terminal = self.create_terminal_with_limit(
            spec,
            TerminalOwner::Agent { session },
            output_byte_limit,
        );
        if let Some(task) = task {
            self.attach_task_run(task, terminal, termesh_core::TaskOrigin::Agent { session });
        }
        let generation = self
            .terminals
            .iter()
            .find(|item| item.id == terminal)
            .expect("new terminal is retained")
            .generation;
        self.pending_terminal_creates.push(PendingTerminalCreate { request, terminal, generation });
    }

    fn respond_terminal(
        &mut self,
        request: AgentTerminalRequestId,
        response: AgentTerminalResponse,
    ) {
        self.agent_outbox.push(AgentRequest::TerminalResponse { request, response });
    }

    fn complete_terminal_waits(&mut self, terminal: TerminalId, response: AgentTerminalResponse) {
        let mut index = 0;
        while index < self.pending_terminal_waits.len() {
            if self.pending_terminal_waits[index].terminal == terminal {
                let pending = self.pending_terminal_waits.remove(index);
                self.respond_terminal(pending.request, response.clone());
            } else {
                index += 1;
            }
        }
    }

    fn cancel_agent_terminals(&mut self, session: SessionId, reason: &str) {
        self.reject_pending_permission(Some(session), reason);
        let pending = std::mem::take(&mut self.pending_terminal_creates);
        for create in pending {
            let owned = self.terminals.iter().any(|terminal| {
                terminal.id == create.terminal
                    && terminal.owner == (TerminalOwner::Agent { session })
            });
            if owned {
                self.respond_terminal(create.request, AgentTerminalResponse::Error(reason.into()));
            } else {
                self.pending_terminal_creates.push(create);
            }
        }

        let terminals: Vec<(TerminalId, TerminalGeneration, bool)> = self
            .terminals
            .iter()
            .filter(|terminal| {
                terminal.owner == (TerminalOwner::Agent { session }) && !terminal.released
            })
            .map(|terminal| {
                (
                    terminal.id,
                    terminal.generation,
                    matches!(
                        terminal.status,
                        TerminalStatus::Starting | TerminalStatus::Running { .. }
                    ),
                )
            })
            .collect();
        for (terminal, generation, running) in terminals {
            if running {
                self.terminal_outbox.push(PtyRequest::Kill { terminal, generation });
            }
            self.terminal_outbox.push(PtyRequest::Release { terminal, generation });
            if let Some(item) = self.terminals.iter_mut().find(|item| item.id == terminal) {
                item.released = true;
            }
            self.complete_terminal_waits(terminal, AgentTerminalResponse::Error(reason.into()));
        }
    }

    fn reject_pending_permission(&mut self, session: Option<SessionId>, reason: &str) {
        let pending = self.agent.as_mut().and_then(|agent| {
            if session.is_none_or(|session| agent.id == session) {
                agent.pending_permission.take()
            } else {
                None
            }
        });
        let Some(pending) = pending else { return };
        match pending.origin {
            PermissionOrigin::AgentRequest { request, .. } => {
                self.agent_outbox.push(AgentRequest::PermissionCancelled { request });
            }
            PermissionOrigin::TerminalCreate { request, .. } => {
                self.respond_terminal(request, AgentTerminalResponse::Error(reason.into()));
            }
        }
    }

    fn remember_permission(&mut self, spec: &TerminalSpec) {
        let remembered = self
            .explorer
            .as_ref()
            .is_some_and(|explorer| self.permission_policy.remember(&explorer.root.path, spec));
        if !remembered {
            self.notification = Some(
                "approved once; cwd or environment made this command unsafe to remember".into(),
            );
        }
    }

    /// Send a turn.
    ///
    /// The context attached is deliberately small — the workspace snapshot the human is
    /// looking at, nothing more. Everything bulky is *pulled* by the agent through
    /// `fs/read_text_file`, which cannot go stale the way a pushed copy would
    /// (ADR-0007 §4).
    /// Ask the agent something, starting a session first if there is not one yet.
    ///
    /// Making the human run `agent.session.new` from the palette before this worked was
    /// bookkeeping the app should do itself — one key should do the obvious thing.
    fn prompt_agent(&mut self) {
        if self.agent_name.is_none() {
            self.notification =
                Some("no agent configured — see ~/.config/termesh/agents.toml".into());
            return;
        }
        if self.agent.as_ref().is_some_and(|a| a.turn_active) {
            self.notification = Some("the agent is still working".into());
            return;
        }
        // Only meaningful once a session exists; a turn typed before then is held in
        // `pending_prompt` and sent the moment one opens.
        let session = self.agent.as_ref().map(|a| a.id).unwrap_or(SessionId::new(0));
        self.overlays.push(Overlay::Prompt(Prompt {
            title: "Ask the agent".into(),
            input: String::new(),
            kind: PromptKind::AgentPrompt { session },
        }));
    }

    /// Send a turn.
    ///
    /// The context attached is deliberately small — the workspace snapshot the human is
    /// looking at, nothing more. Everything bulky is *pulled* by the agent through
    /// `fs/read_text_file`, which cannot go stale the way a pushed copy would
    /// (ADR-0007 §4).
    fn send_prompt(&mut self, session: SessionId, text: String) {
        if let Some(agent) = self.agent.as_mut() {
            agent.turn_active = true;
            agent.transcript.push(TranscriptLine { speaker: Speaker::You, text: text.clone() });
        }
        let context = self.agent_context();
        self.agent_outbox.push(AgentRequest::Prompt { session, text, context });
    }

    /// The workspace snapshot, rendered for the agent (ARCHITECTURE.md §9.2).
    pub fn agent_context(&self) -> String {
        let Some(snapshot) = self.workspace_snapshot() else {
            return "no workspace open".into();
        };
        let open: Vec<String> = self
            .buffers
            .iter()
            .map(|b| {
                let dirty = if b.is_dirty() { " (unsaved)" } else { "" };
                format!("  {}{dirty}", b.display_name())
            })
            .collect();

        let tasks = if self.task_catalog.is_empty() {
            "tasks: (no adapter)".to_string()
        } else {
            let entries = self
                .task_catalog
                .iter()
                .take(20)
                .map(|task| {
                    format!(
                        "  task.run {} ({})\n    program: {}\n    args: {:?}\n    cwd: {}",
                        task.id,
                        task.label,
                        task.program,
                        task.args,
                        task.cwd.display()
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            format!(
                "tasks:\n{entries}\ninvocation: ACP terminal/create with this exact program, args, and cwd; normal permission applies"
            )
        };

        format!(
            "project: {} ({})\nvisible entries: {}\nopen buffers:\n{}\n{}\n{}\n{}",
            snapshot.root.file_name().unwrap_or_default().to_string_lossy(),
            termesh_workspace::kind_labels(&snapshot.project_kinds),
            snapshot.len(),
            if open.is_empty() { "  (none)".to_string() } else { open.join("\n") },
            tasks,
            self.git_agent_context(),
            self.lsp_agent_context(),
        )
    }

    /// Bounded Git state built only from the snapshot already shown to the human.
    pub fn git_agent_context(&self) -> String {
        const LIMIT: usize = 64 * 1024;
        let Some(snapshot) = self.git.snapshot.as_ref() else {
            return match &self.git.load {
                GitLoadState::Loading => "git: loading".into(),
                GitLoadState::Unavailable(failure)
                    if failure.kind == GitFailureKind::NotRepository =>
                {
                    "git: not a repository".into()
                }
                GitLoadState::Unavailable(failure) | GitLoadState::Stale(failure) => {
                    format!("git: unavailable ({})", failure.message)
                }
                GitLoadState::Idle | GitLoadState::Ready => "git: unavailable".into(),
            };
        };

        let mut context = BoundedContext::new(LIMIT, "[git context truncated]");
        let branch = if snapshot.branch.detached {
            snapshot.branch.oid.as_deref().map(|oid| &oid[..oid.len().min(7)]).unwrap_or("detached")
        } else {
            snapshot.branch.head.as_deref().unwrap_or("unborn")
        };
        let upstream = snapshot.branch.upstream.as_deref().unwrap_or("(none)");
        context.line(&format!(
            "git: branch {branch}; upstream {upstream}; ahead {}; behind {}",
            snapshot.branch.ahead, snapshot.branch.behind
        ));

        for row in GitStatusOverlay::new(snapshot, self.focus).rows() {
            let absolute = snapshot.repository_root.join(&row.path);
            if !absolute.starts_with(&snapshot.workspace_root) {
                continue;
            }
            let label = match row.group {
                crate::git_state::GitGroup::Conflicts => "conflicted",
                crate::git_state::GitGroup::Staged => "staged",
                crate::git_state::GitGroup::Changes => "unstaged",
            };
            if !context.line(&format!("{label}: {}", row.path.display())) {
                return context.finish();
            }
        }

        if !context.line("staged diff:") {
            return context.finish();
        }
        if snapshot.context_diff.index.is_empty() {
            context.line("(none)");
        } else if !context.text(&snapshot.context_diff.index) {
            return context.finish();
        }
        if snapshot.context_diff.index_truncated {
            context.line("[staged diff truncated by Git service]");
        }
        if !context.line("worktree diff:") {
            return context.finish();
        }
        if snapshot.context_diff.worktree.is_empty() {
            context.line("(none)");
        } else if !context.text(&snapshot.context_diff.worktree) {
            return context.finish();
        }
        if snapshot.context_diff.worktree_truncated {
            context.line("[worktree diff truncated by Git service]");
        }
        context.finish()
    }

    /// Bounded language intelligence derived from the same diagnostics and symbols the
    /// UI renders. Nothing is fetched separately for the agent.
    pub fn lsp_agent_context(&self) -> String {
        const LIMIT: usize = 64 * 1024;
        let detected: Vec<_> = self
            .explorer
            .iter()
            .flat_map(|explorer| explorer.root.kinds.iter())
            .map(|kind| kind.label())
            .collect();
        if detected.is_empty() && self.lsp.sessions.is_empty() && self.lsp.configured.is_empty() {
            return "language: no language server configured".into();
        }
        let mut context = BoundedContext::new(LIMIT, "[language context truncated]");
        if detected.is_empty() {
            context.line("detected workspace languages: (none)");
        } else {
            context.line(&format!("detected workspace languages: {}", detected.join(", ")));
        }
        if self.lsp.sessions.is_empty() {
            context.line("language servers: not started");
        }
        for session in self.lsp.sessions.values() {
            let status = match &session.load {
                LspLoadState::Idle => "idle".to_string(),
                LspLoadState::Starting => "starting".to_string(),
                LspLoadState::Indexing { message, percent } => match percent {
                    Some(percent) => format!("indexing ({percent}%): {message}"),
                    None => format!("indexing: {message}"),
                },
                LspLoadState::Ready => "ready".to_string(),
                LspLoadState::Unavailable(failure) => {
                    format!("unavailable: {}", failure.message)
                }
                LspLoadState::Stale(failure) => format!("stale: {}", failure.message),
            };
            if !context.line(&format!("language server {}: {status}", session.language)) {
                return context.finish();
            }
        }

        let open_paths: HashSet<_> =
            self.buffers.iter().filter_map(|buffer| buffer.path().map(Path::to_path_buf)).collect();
        let mut diagnostics: Vec<_> = self
            .problem_rows()
            .into_iter()
            .filter(|row| {
                row.origin == termesh_core::DiagnosticOrigin::LanguageServer
                    && open_paths.contains(&row.path)
            })
            .collect();
        diagnostics.sort_by(|left, right| {
            diagnostic_severity_rank(left.severity)
                .cmp(&diagnostic_severity_rank(right.severity))
                .then_with(|| left.path.cmp(&right.path))
                .then_with(|| left.line.cmp(&right.line))
                .then_with(|| left.column.cmp(&right.column))
                .then_with(|| left.message.cmp(&right.message))
        });
        if !context.line("language diagnostics (open buffers):") {
            return context.finish();
        }
        if diagnostics.is_empty() {
            if !context.line("  (none)") {
                return context.finish();
            }
        } else {
            for row in diagnostics {
                if !context.line(&format!(
                    "  {}: {}:{}:{}: {} [{}]",
                    diagnostic_severity_name(row.severity),
                    row.path.display(),
                    row.line,
                    row.column,
                    row.message,
                    row.source
                )) {
                    return context.finish();
                }
            }
        }

        let (errors, warnings) = self.lsp.diagnostics.values().flatten().fold(
            (0usize, 0usize),
            |(errors, warnings), diagnostic| match diagnostic.severity {
                termesh_core::DiagnosticSeverity::Error => (errors + 1, warnings),
                termesh_core::DiagnosticSeverity::Warning => (errors, warnings + 1),
                termesh_core::DiagnosticSeverity::Info | termesh_core::DiagnosticSeverity::Hint => {
                    (errors, warnings)
                }
            },
        );
        if !context
            .line(&format!("workspace language totals: {errors} errors, {warnings} warnings"))
        {
            return context.finish();
        }

        if let Some(path) = self.active_buffer().and_then(Buffer::path) {
            if !context.line(&format!("active document outline: {}", path.display())) {
                return context.finish();
            }
            match self.document_symbols.get(path) {
                Some(symbols) if !symbols.is_empty() => {
                    if !append_symbol_outline(&mut context, symbols, 1) {
                        return context.finish();
                    }
                }
                _ => {
                    context.line("  (not available)");
                }
            }
        }
        context.finish()
    }

    /// Accept or reject the oldest pending proposal.
    ///
    /// Accepting applies every *clean* hunk as **one transaction**, so the whole proposal
    /// is a single undo step (ADR-0006 §6) — which is what makes the exit criterion's
    /// "accept, undo" mean "undo the agent's change" rather than undo one hunk of it.
    /// Conflicted hunks are left behind for the human to resolve or re-ask about.
    fn resolve_proposal(&mut self, accept: bool) {
        // A proposal the agent is holding a permission open for is not ours to apply. The
        // agent writes it as soon as it is allowed to, so accepting here means answering
        // that request — applying it as well would write the change twice, and the second
        // writer would race our own watcher (ADR-0016 §2).
        let gated = self
            .agent
            .as_ref()
            .and_then(|agent| agent.pending_permission.as_ref())
            .and_then(|pending| pending.review)
            .filter(|reviewed| {
                self.agent
                    .as_ref()
                    .and_then(|agent| agent.proposals.first())
                    .is_some_and(|first| first.id == *reviewed)
            });
        if gated.is_some() {
            self.decide_permission(if accept {
                termesh_core::PermissionDecision::AllowOnce
            } else {
                termesh_core::PermissionDecision::RejectOnce
            });
            self.notification = Some(if accept {
                "allowed — the agent is making the change".into()
            } else {
                "rejected — the file is untouched".into()
            });
            return;
        }

        let Some(agent) = self.agent.as_mut() else {
            self.notification = Some("no agent session".into());
            return;
        };
        let Some(proposal) =
            (if agent.proposals.is_empty() { None } else { Some(agent.proposals.remove(0)) })
        else {
            self.notification = Some("nothing to review".into());
            return;
        };

        let total = proposal.hunks.len();
        // A hunk the buffer already contains is `Satisfied`, not `Conflicted` — the agent
        // asked for something that is already true. Counted separately because the two
        // read identically from the applied count alone, and reporting a conflict for a
        // change that is already present sends the reader looking for a problem that is
        // not there. Agents that write to disk themselves rather than through the client
        // produce exactly this: by the time the human accepts, the watcher has reloaded
        // the buffer and the edit is in it.
        let satisfied = proposal
            .hunks
            .iter()
            .filter(|hunk| matches!(hunk.state, termesh_editor::HunkState::Satisfied))
            .count();
        let mut applied = 0;

        if accept {
            if let Some(index) =
                self.buffers.iter().position(|b| b.path() == Some(proposal.path.as_path()))
            {
                let clean: Vec<&termesh_agent::Hunk> = proposal.applicable().collect();
                applied = clean.len();
                if applied > 0 {
                    let len = self.buffers[index].text().len_chars();
                    let changes = termesh_agent::changeset_from_hunks(&clean, len);
                    let tx =
                        self.buffers[index].transaction(changes, EditSource::Agent(proposal.id));
                    if let Err(e) = self.buffers[index].apply(&tx) {
                        self.notification = Some(e.to_string());
                        applied = 0;
                    }
                }
            }
        }

        // The review is over either way, so its decorations go.
        for buffer in &mut self.buffers {
            buffer.decorations_mut().remove_proposal(proposal.id);
        }

        // ADR-0007 §8: the protocol has no partial accept, so anything less than the
        // whole proposal is reported as a rejection rather than letting the agent
        // believe the file now matches what it proposed.
        let decision = termesh_agent::permission_for_review(applied, total);
        self.agent_outbox.push(AgentRequest::Permission {
            request: PermissionRequestId::new(proposal.id.0),
            decision,
        });

        self.notification = Some(match (accept, applied, total) {
            (false, _, _) => format!("rejected {total} edit(s)"),
            (true, 0, t) if satisfied == t => {
                "nothing to apply — the buffer already matches what was proposed".to_string()
            }
            (true, 0, _) => "nothing applied — every hunk conflicted".to_string(),
            (true, a, t) if a + satisfied == t => {
                format!("applied {a} edit(s); {satisfied} already present")
            }
            (true, a, t) if a < t => format!("applied {a} of {t} edit(s); the rest conflicted"),
            (true, a, _) => format!("applied {a} edit(s)"),
        });
    }

    /// Answer a pending permission request.
    pub fn decide_permission(&mut self, decision: termesh_core::PermissionDecision) {
        let Some(agent) = self.agent.as_mut() else { return };
        let Some(pending) = agent.pending_permission.take() else { return };

        // The agent makes this edit itself the moment it is allowed to, so the client's copy
        // of it is a display artefact and goes away with the prompt either way. Applying it
        // on accept would write the same change twice (ADR-0016 §2).
        if let Some(reviewed) = pending.review {
            agent.proposals.retain(|proposal| proposal.id != reviewed);
            self.sync_proposals();
        }

        match pending.origin {
            PermissionOrigin::AgentRequest { request, terminal_spec } => {
                if decision == termesh_core::PermissionDecision::AllowAlways {
                    if let Some(spec) = terminal_spec.as_ref() {
                        self.remember_permission(spec);
                    }
                }
                self.agent_outbox.push(AgentRequest::Permission { request, decision });
            }
            PermissionOrigin::TerminalCreate { session, request, spec, output_byte_limit } => {
                if decision.allows() {
                    if decision == termesh_core::PermissionDecision::AllowAlways {
                        self.remember_permission(&spec);
                    }
                    self.spawn_agent_terminal(session, request, spec, output_byte_limit);
                } else {
                    self.respond_terminal(
                        request,
                        AgentTerminalResponse::Error("command rejected".into()),
                    );
                }
            }
        }
        if self.notification.is_none() {
            self.notification = Some(if decision.allows() {
                format!("approved: {}", pending.summary)
            } else {
                format!("denied: {}", pending.summary)
            });
        }
    }

    /// Add to the conversation, continuing the current speaker's turn where possible.
    fn push_transcript(&mut self, speaker: Speaker, text: &str) {
        let Some(agent) = self.agent.as_mut() else { return };

        match agent.transcript.last_mut() {
            // Still the same speaker: this is the next fragment of one sentence, not a
            // new line. Appending is what makes streamed prose read as prose.
            Some(last) if last.speaker == speaker => last.text.push_str(text),
            _ => agent.transcript.push(TranscriptLine { speaker, text: text.to_string() }),
        }
        // A new turn means the interesting thing is at the bottom again.
        self.agent_scroll = 0;

        if agent.transcript.len() > TRANSCRIPT_LIMIT {
            agent.transcript.remove(0);
        }
    }

    /// Answer `fs/read_text_file` — **from the live buffer** when the file is open
    /// (ADR-0007 §3), from disk through the filesystem worker otherwise (ADR-0014 Task 1).
    ///
    /// This is the shared state, concretely: if the file is open, the agent sees exactly
    /// what the human sees, unsaved edits included. Recording what we served is what lets
    /// the resulting proposal anchor to a revision we hold rather than to text we have to
    /// guess at. A file nobody has opened is still part of the workspace the agent was
    /// handed, so it is read rather than refused — refusing it is what made `initialize`'s
    /// `readTextFile: true` a lie for every file the human had not clicked on first.
    fn serve_read(&mut self, session: SessionId, request: ReadRequestId, path: PathBuf) {
        if let Some(buffer) = self.buffers.iter().find(|b| b.path() == Some(path.as_path())) {
            let text = buffer.text().to_string();
            // Only the newest entry per path is ever consulted, and an unbounded
            // history of every version served would grow for the life of the session
            // (ARCHITECTURE.md §19 — agent-context caches stay bounded).
            self.served_reads.retain(|r| r.path != path);
            self.served_reads.push(ServedRead {
                path: path.clone(),
                version: buffer.version(),
                text: text.clone(),
            });
            self.agent_outbox.push(AgentRequest::FileContents {
                session,
                request,
                path,
                contents: Some(text),
            });
            return;
        }

        // The agent has a workspace, not a filesystem: no root open, or the path steps
        // outside the one root it was handed, both answer empty rather than reaching disk.
        let inside_root =
            self.explorer.as_ref().is_some_and(|explorer| path.starts_with(&explorer.root.path));
        if !inside_root {
            self.agent_outbox.push(AgentRequest::FileContents {
                session,
                request,
                path,
                contents: None,
            });
            return;
        }

        // Not open: read it through the worker, the same way opening a file does
        // (ADR-0005 §1 — a cold file on a network mount must not freeze the UI). Kept out
        // of `self.opening` and `self.buffers` deliberately: the human never asked to open
        // this file, only the agent asked to read it, so no buffer, tab, or focus change
        // should follow from it. Also *not* recorded as a served read once it arrives — the
        // resulting proposal must anchor by content instead of pretending to a version we
        // never took.
        self.next_buffer_id += 1;
        let buffer = BufferId::new(self.next_buffer_id);
        self.pending_agent_reads.push(PendingAgentRead {
            buffer,
            session,
            request,
            path: path.clone(),
        });
        self.outbox.push(FsRequest::ReadFile { buffer, path });
    }

    /// Prepare the diff behind an edit permission, so the human answers with the change in
    /// front of them rather than a file path (ADR-0016 §1).
    ///
    /// Returns the proposal to display, if one could be built, and the summary to show —
    /// which gains a reason whenever it could not. Nothing here writes: the agent performs
    /// this edit itself once permitted, and applying it as well would write it twice
    /// (ADR-0016 §2).
    fn review_for_permission(
        &mut self,
        request: PermissionRequestId,
        edit: termesh_core::ProposedEditDiff,
        summary: String,
    ) -> (Option<ProposalId>, String) {
        let name = display_name(&edit.path);
        let Some(index) = self.buffers.iter().position(|b| b.path() == Some(edit.path.as_path()))
        else {
            return (None, format!("{summary} (open {name} to see the diff)"));
        };

        let current = self.buffers[index].text().to_string();
        let whole = match termesh_agent::whole_file_from_permission_diff(
            &current,
            &edit.old_text,
            &edit.new_text,
        ) {
            Ok(whole) => whole,
            // Never guess an offset. The human still has to answer, so say which kind of
            // uncertainty this is: one means the file moved, the other means the agent's
            // description fits more than one place in it.
            Err(termesh_agent::AnchorFailure::NotFound) => {
                return (None, format!("{summary} ({name} has changed since the agent read it)"));
            }
            Err(termesh_agent::AnchorFailure::Ambiguous) => {
                return (None, format!("{summary} (the change matches several places in {name})"));
            }
        };

        // The permission's own number, which the agent's id counter has already spent on
        // this request and will not hand to a proposal. Minting from a second counter here
        // would eventually collide with an agent-issued `ProposalId` and silently replace
        // somebody else's pending review.
        let id = ProposalId::new(request.0);
        // Anchored against the buffer as it stands, so `old_text` is the buffer itself and
        // the derived hunks are exactly the change under review.
        let proposal = EditProposal::new(id, edit.path, None, current.clone(), whole, &current);

        if let Some(agent) = self.agent.as_mut() {
            agent.proposals.push(proposal);
        }
        self.sync_proposals();
        (Some(id), summary)
    }

    /// Turn a whole-file diff into a reviewable proposal anchored onto the buffer.
    fn receive_proposal(
        &mut self,
        id: ProposalId,
        path: PathBuf,
        old_text: Option<String>,
        new_text: String,
    ) {
        let Some(index) = self.buffers.iter().position(|b| b.path() == Some(path.as_path())) else {
            // A proposal for a file we have not opened. Opening it is the right move —
            // review has to happen somewhere the human can see it — but that is a read
            // round trip, so for now say so rather than dropping it silently.
            self.notification =
                Some(format!("agent proposed edits to {}; open it to review", display_name(&path)));
            return;
        };

        let current = self.buffers[index].text().to_string();

        // A `fs/write_text_file` says what the file should become, not what it was. The
        // client owns the buffer, so the client supplies the base — using an empty string
        // would diff the whole file as one insertion and make every write a rewrite.
        let old_text = old_text.unwrap_or_else(|| current.clone());

        let base_version = self
            .served_reads
            .iter()
            .rev()
            .find(|r| r.path == path && r.text == old_text)
            .map(|r| r.version);
        let proposal = EditProposal::new(id, path, base_version, old_text, new_text, &current);

        if let Some(agent) = self.agent.as_mut() {
            agent.proposals.retain(|p| p.id != id);
            agent.proposals.push(proposal);
        }
        self.sync_proposals();
    }

    /// Files above this are left unhighlighted.
    ///
    /// Highlighting reparses the whole file on every edit (see `termesh_syntax`), which is
    /// imperceptible for ordinary source and not for a megabyte of generated code. A large
    /// file that types smoothly without colour beats one that stutters with it — the cap
    /// goes away when parsing is incremental.
    const HIGHLIGHT_LIMIT: usize = 256 * 1024;

    /// Re-highlight the active buffer.
    pub fn sync_syntax(&mut self) {
        let Some(index) = self.active_buffer else { return };
        let Some(path) = self.buffers[index].path().map(Path::to_path_buf) else { return };

        let Some(language) = termesh_syntax::Language::from_path(&path) else {
            self.buffers[index].decorations_mut().clear_syntax();
            return;
        };
        let text = self.buffers[index].text().to_string();
        if text.len() > Self::HIGHLIGHT_LIMIT {
            self.buffers[index].decorations_mut().clear_syntax();
            return;
        }

        // Loading a grammar is expensive; keep the one we have while the language holds.
        if self.highlighter.as_ref().map(|(l, _)| *l) != Some(language) {
            self.highlighter = termesh_syntax::Highlighter::new(language).map(|h| (language, h));
        }
        let Some((_, highlighter)) = self.highlighter.as_mut() else { return };
        let spans = highlighter.highlight(&text);

        let decorations = self.buffers[index].decorations_mut();
        decorations.clear_syntax();
        for (start, end, kind) in spans {
            decorations.push(Decoration::new(start, end, DecorationClass::Syntax(kind)));
        }
    }

    /// Re-anchor every live proposal to its buffer and redraw its hunks.
    ///
    /// [`EditProposal`] is the single owner of review state; the decorations in a buffer
    /// are a **projection** of it, thrown away and rebuilt here rather than maintained in
    /// parallel. Two mechanisms tracking one piece of state is how a proposal ends up
    /// reading clean in the pane and conflicted in the gutter.
    /// Offer the agent's modes, if it published any.
    ///
    /// Says which of the three situations applies rather than doing nothing: no session,
    /// a session whose agent offers no choice, or a list to pick from. An agent with no
    /// modes is the common case and not a fault (ADR-0015 §4).
    fn open_agent_modes(&mut self) {
        let Some(agent) = self.agent.as_ref() else {
            self.notification = Some("no agent session".into());
            return;
        };
        if agent.modes.is_empty() {
            self.notification = Some("this agent offers no session modes".into());
            return;
        }
        let current = agent.current_mode.clone();
        let selected = current
            .as_ref()
            .and_then(|id| agent.modes.iter().position(|mode| &mode.id == id))
            .unwrap_or(0);
        self.overlays.push(Overlay::AgentModes(AgentModePicker {
            modes: agent.modes.clone(),
            current,
            selected,
        }));
    }

    /// Ask the agent to change mode. The client's own view is not updated here — it moves
    /// when the agent says it moved (ADR-0015 §5).
    pub fn set_agent_mode(&mut self, mode: String) {
        let Some(agent) = self.agent.as_ref() else { return };
        self.agent_outbox.push(AgentRequest::SetMode { session: agent.id, mode });
    }

    pub fn sync_proposals(&mut self) {
        let Some(agent) = self.agent.as_mut() else { return };
        if agent.proposals.is_empty() {
            return;
        }

        // Re-derive each proposal from its immutable original against the live text.
        for proposal in agent.proposals.iter_mut() {
            if let Some(buffer) =
                self.buffers.iter().find(|b| b.path() == Some(proposal.path.as_path()))
            {
                proposal.refresh(&buffer.text().to_string());
            }
        }

        // Then repaint. Every hunk decoration is discarded first, so a hunk that has
        // resolved or moved cannot leave a stale marker behind.
        for buffer in self.buffers.iter_mut() {
            for proposal in agent.proposals.iter() {
                buffer.decorations_mut().remove_proposal(proposal.id);
            }
            let Some(path) = buffer.path().map(Path::to_path_buf) else { continue };
            for proposal in agent.proposals.iter().filter(|p| p.path == path) {
                for hunk in &proposal.hunks {
                    let side =
                        if hunk.is_insertion() { HunkSide::Added } else { HunkSide::Removed };
                    buffer.decorations_mut().push(Decoration::new(
                        hunk.start,
                        hunk.end,
                        DecorationClass::Hunk { proposal: proposal.id, side, state: hunk.state },
                    ));
                }
            }
        }
    }

    /// Open a workspace and satisfy its reads immediately, on this thread.
    ///
    /// For `--dump-frame` and tests only: a headless frame must be *complete* and
    /// deterministic rather than catching the tree mid-load, and blocking is free when
    /// there is no user waiting. The interactive path uses [`Self::open_workspace`] and
    /// lets the worker answer.
    pub fn open_workspace_sync(&mut self, fs: &dyn FileSystemService, path: &Path) {
        let root = termesh_workspace::detect_root(fs, path);
        let root_path = root.path.clone();
        self.open_workspace_configured(fs, root);
        let mut reader = DirReader::new(fs, &root_path, self.ignore_options);
        self.settle_fs_sync(&mut reader);
    }

    /// Load workspace-local language settings and open `root`, falling back to the
    /// built-in recipe while surfacing malformed or unreadable configuration.
    pub fn open_workspace_configured(&mut self, fs: &dyn FileSystemService, root: WorkspaceRoot) {
        let language_settings = match LanguageSettings::load(fs, &root.path) {
            Ok(settings) => settings,
            Err(error) => {
                self.notification = Some(error.to_string());
                LanguageSettings::default()
            }
        };
        let mut task_catalog = self.task_service.catalog(&root, fs);
        task_catalog.extend(language_settings.tasks(&root.path));
        self.open_workspace_with_language(root, language_settings, task_catalog);
    }

    /// Begin best-effort restoration. Workspace metadata is applied now; buffer contents
    /// still travel through the filesystem request queue, so the first frame is not held
    /// behind file reads.
    pub fn restore_session(&mut self, fs: &dyn FileSystemService, session: &Session) {
        let Some(workspace) = session.workspace.as_ref() else { return };
        if workspace.root.as_os_str().is_empty() {
            return;
        }
        let root = termesh_workspace::detect_root(fs, &workspace.root);
        self.open_workspace_configured(fs, root);
        self.layout.sidebar_pct = workspace.layout.sidebar_pct.clamp(10, 45);
        self.layout.bottom_pct = workspace.layout.bottom_pct.clamp(10, 45);
        self.layout.agent_pct = workspace.layout.agent_pct.clamp(10, 45);
        let history_start = workspace.agent_history.len().saturating_sub(TRANSCRIPT_LIMIT);
        self.restored_agent_history = workspace.agent_history[history_start..]
            .iter()
            .map(|line| TranscriptLine {
                speaker: match line.speaker {
                    AgentHistorySpeaker::You => Speaker::You,
                    AgentHistorySpeaker::Agent => Speaker::Agent,
                    AgentHistorySpeaker::Thought => Speaker::Thought,
                },
                text: line.text.clone(),
            })
            .collect();
        let mut pending = HashSet::new();
        for path in &workspace.open {
            self.open_file(path.clone());
            if let Some((buffer, _)) = self.opening.iter().find(|(_, opening)| opening == path) {
                pending.insert(*buffer);
            }
        }
        if !pending.is_empty() {
            self.pending_session_restore = Some(PendingSessionRestore {
                order: workspace.open.clone(),
                active: workspace.active.clone(),
                pending,
            });
        }
        let restored_focus = self.focus;
        let restored_previous_focus = self.previous_non_terminal_focus;
        for cwd in &workspace.terminals {
            let spec = termesh_platform::shell(self.settings.shell.as_deref(), cwd);
            self.create_terminal(spec, TerminalOwner::HumanShell);
        }
        self.focus = restored_focus;
        self.previous_non_terminal_focus = restored_previous_focus;
    }

    /// Copy the restart-owned subset of the live model into an existing session. Reusing
    /// the prior nested value for the same root preserves future-version fields carried
    /// by its flattened map (ADR-0014 §2).
    pub fn persist_session(&self, session: &mut Session) {
        let Some(root) = self.explorer.as_ref().map(|explorer| explorer.root.path.clone()) else {
            return;
        };
        let mut workspace = session
            .workspace
            .take()
            .filter(|workspace| workspace.root == root)
            .unwrap_or_else(|| RestoredWorkspace::new(root.clone()));
        workspace.root = root.clone();
        workspace.open =
            self.buffers.iter().filter_map(|buffer| buffer.path().map(Path::to_path_buf)).collect();
        workspace.active =
            self.active_buffer().and_then(|buffer| buffer.path().map(Path::to_path_buf));
        workspace.layout.set_percentages(
            self.layout.sidebar_pct,
            self.layout.bottom_pct,
            self.layout.agent_pct,
        );
        workspace.terminals = self
            .terminals
            .iter()
            .filter(|terminal| !matches!(terminal.owner, TerminalOwner::Agent { .. }))
            .map(|terminal| terminal.spec.cwd.clone())
            .collect();

        let mut history = self.restored_agent_history.clone();
        if let Some(agent) = &self.agent {
            history.extend(agent.transcript.iter().cloned());
        }
        let history_start = history.len().saturating_sub(TRANSCRIPT_LIMIT);
        workspace.agent_history = history[history_start..]
            .iter()
            .map(|line| AgentHistoryLine {
                speaker: match line.speaker {
                    Speaker::You => AgentHistorySpeaker::You,
                    Speaker::Agent => AgentHistorySpeaker::Agent,
                    Speaker::Thought => AgentHistorySpeaker::Thought,
                },
                text: line.text.clone(),
            })
            .collect();

        session.record(&root);
        session.workspace = Some(workspace);
    }

    /// ACP has no load operation in this client. Once configuration has selected an
    /// agent, request a genuinely new session while leaving restored history display-only.
    pub fn start_fresh_agent_after_restore(&mut self) {
        if !self.restored_agent_history.is_empty()
            && self.agent_name.is_some()
            && self.agent.is_none()
        {
            self.new_agent_session();
        }
    }

    fn finish_restored_read(&mut self, buffer: BufferId) {
        let Some(restore) = self.pending_session_restore.as_mut() else { return };
        restore.pending.remove(&buffer);
        let order = restore.order.clone();
        let active = restore.active.clone();
        let complete = restore.pending.is_empty();

        self.buffers.sort_by_key(|candidate| {
            candidate
                .path()
                .and_then(|path| order.iter().position(|restored| restored == path))
                .unwrap_or(usize::MAX)
        });
        self.active_buffer = active
            .as_deref()
            .and_then(|path| {
                self.buffers.iter().position(|candidate| candidate.path() == Some(path))
            })
            .or_else(|| self.buffers.len().checked_sub(1));
        if complete {
            self.pending_session_restore = None;
        }
    }

    fn append_notification(&mut self, message: impl AsRef<str>) {
        let message = message.as_ref();
        match &mut self.notification {
            Some(existing) if !existing.is_empty() => {
                existing.push('\n');
                existing.push_str(message);
            }
            _ => self.notification = Some(message.to_string()),
        }
    }

    #[cfg(test)]
    pub fn session_restore_pending(&self) -> bool {
        self.pending_session_restore.is_some()
    }

    /// Open a file and satisfy the read immediately, on this thread.
    ///
    /// The headless counterpart of [`Self::open_file`], for `--dump-frame` and tests:
    /// a printed frame must show the file, not catch it mid-load.
    pub fn open_file_sync(&mut self, fs: &dyn FileSystemService, path: PathBuf) {
        let root = self
            .explorer
            .as_ref()
            .map(|e| e.root.path.clone())
            .unwrap_or_else(|| path.parent().unwrap_or(Path::new(".")).to_path_buf());

        self.open_file(path);
        let mut reader = DirReader::new(fs, &root, self.ignore_options);
        self.settle_fs_sync(&mut reader);
    }

    /// Serve every queued read through `reader` until the model stops asking for more.
    ///
    /// Takes a [`DirReader`] rather than a raw service so ignore rules apply here exactly
    /// as they do on the worker thread — otherwise headless frames and tests would be
    /// asserting a tree the running app never shows.
    pub fn settle_fs_sync(&mut self, reader: &mut DirReader<'_>) {
        // Bounded: absorbing one listing can queue further reads, and a cycle here would
        // hang a headless render rather than fail it.
        for _ in 0..64 {
            let requests = self.take_fs_requests();
            if requests.is_empty() {
                return;
            }
            for request in requests {
                match request {
                    FsRequest::ReadDir { id, path } => {
                        let event = match reader.read(&path) {
                            Ok(entries) => FsEvent::DirLoaded { id, entries },
                            Err(error) => FsEvent::DirFailed { id, error },
                        };
                        self.on_fs_event(event);
                    }
                    // Mirrors the worker: success reports as `Changed`, so the tree
                    // refreshes through the same path either way.
                    FsRequest::CreateFile(path) => {
                        let outcome = reader.service().create_file(&path);
                        self.on_fs_event(mutation_event(outcome, path));
                    }
                    FsRequest::CreateDir(path) => {
                        let outcome = reader.service().create_dir(&path);
                        self.on_fs_event(mutation_event(outcome, path));
                    }
                    FsRequest::Rename { from, to } => {
                        let event = match reader.service().rename(&from, &to) {
                            Ok(()) => FsEvent::Changed(vec![from, to]),
                            Err(e) => FsEvent::MutationFailed(e),
                        };
                        self.on_fs_event(event);
                    }
                    FsRequest::Remove { path, recursive } => {
                        let outcome = if recursive {
                            reader.service().remove_dir_all(&path)
                        } else {
                            reader.service().remove_file(&path)
                        };
                        self.on_fs_event(mutation_event(outcome, path));
                    }
                    // Mirrors the worker so headless frames and tests exercise the same
                    // open/save path the running app does.
                    FsRequest::ReadFile { buffer, path } => {
                        let event = match reader.service().read_file(&path) {
                            Ok(contents) => FsEvent::FileLoaded { buffer, path, contents },
                            Err(error) => FsEvent::FileFailed { buffer, error },
                        };
                        self.on_fs_event(event);
                    }
                    FsRequest::ReadPreview { request, path, line, context } => {
                        let event = match reader.service().read_file(&path) {
                            Ok(contents) => match String::from_utf8(contents) {
                                Ok(contents) => {
                                    let (start_line, text) =
                                        preview_window(&contents, line, context.min(10));
                                    FsEvent::PreviewLoaded { request, path, start_line, text }
                                }
                                Err(_) => FsEvent::PreviewFailed {
                                    request,
                                    error: termesh_core::FsError::Other {
                                        path: path.clone(),
                                        message: "not a UTF-8 text file".into(),
                                    },
                                    path,
                                },
                            },
                            Err(error) => FsEvent::PreviewFailed { request, path, error },
                        };
                        self.on_fs_event(event);
                    }
                    FsRequest::ResolvePath { request, path } => {
                        let event = match reader.service().canonicalize(&path) {
                            Ok(path) => FsEvent::PathResolved { request, path },
                            Err(error) => FsEvent::PathResolveFailed { request, path, error },
                        };
                        self.on_fs_event(event);
                    }
                    FsRequest::WriteFile { buffer, path, contents, version } => {
                        let event = match reader.service().write_file(&path, &contents) {
                            Ok(()) => FsEvent::FileSaved { buffer, version },
                            Err(error) => FsEvent::FileFailed { buffer, error },
                        };
                        self.on_fs_event(event);
                    }
                    // Watching is a no-op without a worker to do it.
                    FsRequest::Watch(_) | FsRequest::Shutdown => {}
                }
            }
        }
    }

    /// Absorb a result from the filesystem worker.
    pub fn on_fs_event(&mut self, event: FsEvent) {
        let refresh_git = match &event {
            FsEvent::Changed(paths) => self.explorer.as_ref().is_some_and(|explorer| {
                paths.iter().any(|path| path.starts_with(&explorer.root.path))
            }),
            _ => false,
        };
        if let FsEvent::Changed(paths) = &event {
            self.notify_lsp_watched_files(paths);
        }
        // Buffer results are independent of the explorer — a file can be open with no
        // workspace, so these are handled before the explorer guard below.
        match event {
            FsEvent::FileLoaded { buffer, path, contents } => {
                if let Some(i) =
                    self.pending_config_reloads.iter().position(|pending| pending.buffer == buffer)
                {
                    let pending = self.pending_config_reloads.remove(i);
                    match pending.kind {
                        ConfigReloadKind::Settings => {
                            self.apply_settings_bytes(contents, &pending.path)
                        }
                        ConfigReloadKind::Keymap => {
                            self.apply_keymap_bytes(contents, &pending.path)
                        }
                    }
                    return;
                }
                if let Some(i) =
                    self.pending_agent_reads.iter().position(|pending| pending.buffer == buffer)
                {
                    let pending = self.pending_agent_reads.remove(i);
                    // A non-UTF-8 file answers empty rather than mangling bytes into the
                    // transcript — the same rule the open-file path applies below.
                    let contents = String::from_utf8(contents).ok();
                    self.agent_outbox.push(AgentRequest::FileContents {
                        session: pending.session,
                        request: pending.request,
                        path: pending.path,
                        contents,
                    });
                    return;
                }
                let restored = self
                    .pending_session_restore
                    .as_ref()
                    .is_some_and(|restore| restore.pending.contains(&buffer));
                self.opening.retain(|(id, _)| *id != buffer);
                match String::from_utf8(contents) {
                    Ok(text) => {
                        // A reload replaces the buffer in place, keeping its tab position
                        // and its id, so the file does not appear twice and whatever the
                        // agent proposed against it still refers to the same buffer.
                        if let Some(i) = self.buffers.iter().position(|b| b.id() == buffer) {
                            if let Some(old_path) = self.buffers[i].path().map(Path::to_path_buf) {
                                self.close_lsp_document(&old_path);
                            }
                            let cursor = self.buffers[i].selection().clone();
                            self.buffers[i] = Buffer::from_text(buffer, Some(path), &text);
                            self.buffers[i].set_selection(cursor);
                            self.sync_syntax();
                            self.sync_proposals();
                            self.sync_lsp_documents();
                            if let Some(pending) = self.pending_workspace_edit.as_mut() {
                                pending.waiting.remove(&buffer);
                            }
                            self.finish_pending_workspace_edit();
                            return;
                        }
                        self.buffers.push(Buffer::from_text(buffer, Some(path.clone()), &text));
                        if restored {
                            self.finish_restored_read(buffer);
                        } else {
                            self.active_buffer = Some(self.buffers.len() - 1);
                            // Only follow the file if the user is still where they asked for
                            // it. A slow read must not yank focus out from under them.
                            if self.focus == Pane::Project {
                                self.focus = Pane::Editor;
                            }
                        }
                        self.sync_syntax();
                        if self
                            .pending_open_location
                            .as_ref()
                            .is_some_and(|(pending, _, _)| *pending == path)
                        {
                            let (_, line, column) = self.pending_open_location.take().unwrap();
                            self.position_open_buffer(&path, line, column);
                        }
                        self.sync_lsp_documents();
                        if let Some(pending) = self.pending_workspace_edit.as_mut() {
                            pending.waiting.remove(&buffer);
                        }
                        self.finish_pending_workspace_edit();
                    }
                    // Refused by name rather than mangled: V1 edits UTF-8 (§10).
                    Err(_) => {
                        let message = format!("not a text file: {}", display_name(&path));
                        if restored {
                            self.finish_restored_read(buffer);
                            self.append_notification(message);
                        } else if self
                            .pending_workspace_edit
                            .as_ref()
                            .is_some_and(|pending| pending.waiting.contains_key(&buffer))
                        {
                            self.abandon_pending_workspace_edit(format!(
                                "{message}; no language-server edits were applied"
                            ));
                        } else {
                            self.notification = Some(message);
                        }
                    }
                }
                return;
            }
            FsEvent::FileSaved { buffer, version } => {
                if let Some(source) = self.pending_draft_writes.remove(&buffer) {
                    self.known_drafts.insert(source);
                    return;
                }
                let saved = self
                    .buffers
                    .iter()
                    .find(|candidate| candidate.id() == buffer)
                    .and_then(|candidate| candidate.path().map(Path::to_path_buf));
                if let Some(b) = self.buffer_mut(buffer) {
                    b.mark_saved(termesh_editor::Version(version));
                }
                if let Some(path) = saved {
                    self.draft_versions.remove(&path);
                    if self.known_drafts.remove(&path) {
                        if let Some(drafts_dir) = &self.drafts_dir {
                            self.outbox.push(FsRequest::Remove {
                                path: drafts_dir
                                    .join(termesh_workspace::drafts::draft_file_name(&path)),
                                recursive: false,
                            });
                        }
                    }
                    if let Some(server) = self.lsp.server_for(&path) {
                        self.lsp_outbox.push((server, LspRequest::DidSave { path }));
                    }
                }
                return;
            }
            FsEvent::FileFailed { buffer, error } => {
                if let Some(source) = self.pending_draft_writes.remove(&buffer) {
                    self.append_notification(format!(
                        "could not write recovery draft for {}: {error}",
                        source.display()
                    ));
                    return;
                }
                if let Some(i) =
                    self.pending_config_reloads.iter().position(|pending| pending.buffer == buffer)
                {
                    let pending = self.pending_config_reloads.remove(i);
                    // A file that vanished since the last load reloads to the compiled
                    // defaults, the same as one that was never there — reload reflects
                    // what is on disk right now, not what used to be.
                    self.apply_config_read_error(pending.kind, &pending.path, error);
                    return;
                }
                if let Some(i) =
                    self.pending_agent_reads.iter().position(|pending| pending.buffer == buffer)
                {
                    let pending = self.pending_agent_reads.remove(i);
                    self.agent_outbox.push(AgentRequest::FileContents {
                        session: pending.session,
                        request: pending.request,
                        path: pending.path,
                        contents: None,
                    });
                    return;
                }
                let restored = self
                    .pending_session_restore
                    .as_ref()
                    .is_some_and(|restore| restore.pending.contains(&buffer));
                self.opening.retain(|(id, _)| *id != buffer);
                if restored {
                    self.finish_restored_read(buffer);
                    self.append_notification(format!("could not restore buffer: {error}"));
                    return;
                }
                if self
                    .pending_workspace_edit
                    .as_ref()
                    .is_some_and(|pending| pending.waiting.contains_key(&buffer))
                {
                    self.abandon_pending_workspace_edit(format!(
                        "Could not open a language-server edit target: {error}; no files were changed"
                    ));
                } else {
                    self.notification = Some(error.to_string());
                }
                return;
            }
            FsEvent::PreviewLoaded { request, path, start_line, text } => {
                if let Some(Overlay::Search(search)) = self.overlays.last_mut() {
                    if search.preview_request() == Some(request) {
                        let line = search.selected().and_then(|found| found.line).unwrap_or(1);
                        search.set_preview(path, line, start_line, text);
                    }
                }
                return;
            }
            FsEvent::PreviewFailed { request, path, error } => {
                if let Some(Overlay::Search(search)) = self.overlays.last_mut() {
                    if search.preview_request() == Some(request) {
                        let line = search.selected().and_then(|found| found.line).unwrap_or(1);
                        search.set_preview(path, line, 1, error.to_string());
                    }
                }
                return;
            }
            FsEvent::PathResolved { request, path } => {
                if !self
                    .pending_problem_navigation
                    .as_ref()
                    .is_some_and(|pending| pending.request == request)
                {
                    return;
                }
                let pending = self.pending_problem_navigation.take().unwrap();
                let inside = self
                    .explorer
                    .as_ref()
                    .is_some_and(|explorer| path.starts_with(&explorer.root.path));
                if !inside {
                    self.notification =
                        Some(format!("problem path is outside workspace: {}", path.display()));
                    return;
                }
                self.open_file_at(path, pending.problem.line, pending.problem.column);
                return;
            }
            FsEvent::PathResolveFailed { request, error, .. } => {
                if self
                    .pending_problem_navigation
                    .as_ref()
                    .is_some_and(|pending| pending.request == request)
                {
                    self.pending_problem_navigation = None;
                    self.notification = Some(error.to_string());
                }
                return;
            }
            _ => {}
        }

        let exclusions = &self.settings.exclusions;
        let Some(explorer) = self.explorer.as_mut() else { return };
        match event {
            FsEvent::DirLoaded { id, entries } => {
                // A second, user-declared filter on top of `.gitignore` (ADR-0014 Task
                // 3): the low-level reader already applied ignore-file rules, this
                // applies config.toml's `exclusions` the same way.
                let entries = if exclusions.is_empty() {
                    entries
                } else {
                    let root = explorer.root.path.clone();
                    entries
                        .into_iter()
                        .filter(|entry| {
                            !termesh_filesystem::matches_exclusion(
                                &root,
                                exclusions,
                                &entry.path,
                                entry.kind == termesh_core::EntryKind::Dir,
                            )
                        })
                        .collect()
                };
                explorer.tree.set_children(id, entries)
            }
            FsEvent::DirFailed { id, error } => {
                // A refresh of a path that has just been renamed or deleted races the
                // refresh of its parent, which is what actually removes it. That is
                // ordinary churn, not something to interrupt the user about — but every
                // other failure gets surfaced, since a silently failed expansion is
                // indistinguishable from an empty directory.
                if !matches!(error, termesh_core::FsError::NotFound(_)) {
                    self.notification = Some(error.to_string());
                }
                explorer.tree.set_error(id, error);
            }
            FsEvent::MutationFailed(error) => {
                // Nothing changed on disk, so there is nothing to refresh — just say why.
                self.notification = Some(error.to_string());
            }
            FsEvent::Changed(paths) => {
                // Re-read the affected levels; reconciliation in `set_children` keeps
                // ids, expansion, and selection intact (ADR-0005 §5).
                for id in explorer.tree.dirs_to_refresh(&paths) {
                    if let Some(path) = explorer.tree.refresh(id) {
                        self.outbox.push(FsRequest::ReadDir { id, path });
                    }
                }
                self.reload_changed_buffers(&paths);
            }
            // Handled above, before the explorer guard.
            FsEvent::FileLoaded { .. }
            | FsEvent::FileSaved { .. }
            | FsEvent::FileFailed { .. }
            | FsEvent::PreviewLoaded { .. }
            | FsEvent::PreviewFailed { .. }
            | FsEvent::PathResolved { .. }
            | FsEvent::PathResolveFailed { .. } => {}
        }
        if refresh_git {
            self.request_git_refresh();
        }
    }

    /// Re-read open files that changed on disk underneath us.
    ///
    /// An editor that shows stale text after something else edited the file is lying
    /// about the file. A buffer with **unsaved work** is left alone and flagged instead —
    /// silently replacing what the human typed would be worse than showing them something
    /// out of date, and only they can decide which version wins.
    fn reload_changed_buffers(&mut self, paths: &[PathBuf]) {
        let mut stale = Vec::new();
        for path in paths {
            let Some(buffer) = self.buffers.iter().find(|b| b.path() == Some(path.as_path()))
            else {
                continue;
            };
            if buffer.is_dirty() {
                self.notification =
                    Some(format!("{} changed on disk; you have unsaved edits", display_name(path)));
                continue;
            }
            stale.push((buffer.id(), path.clone()));
        }
        for (buffer, path) in stale {
            self.outbox.push(FsRequest::ReadFile { buffer, path });
        }
    }

    /// Explorer commands act only while the Project pane has focus.
    ///
    /// This is a stand-in for real keymap context predicates: with no editor yet there is
    /// nothing else competing for the arrow keys. Phase 03 introduces a second consumer
    /// and should replace this with a context-aware `Keymap::resolve` rather than adding
    /// a second focus check here.
    fn explorer_focused(&mut self) -> Option<&mut FileTree> {
        if self.focus != Pane::Project {
            return None;
        }
        self.explorer.as_mut().map(|e| &mut e.tree)
    }

    // --- file operations ---------------------------------------------------------
    //
    // Every one of these goes through the action registry (CONTRIBUTING.md "one command
    // surface"), so the palette reaches them today and the agent's ACP tool surface
    // reaches the same code in Phase 03 — permission-gated, since they all write.

    /// The directory a new entry should go into: the selection if it is a directory,
    /// otherwise the directory containing it.
    fn target_dir(&self) -> Option<PathBuf> {
        let tree = &self.explorer.as_ref()?.tree;
        let node = tree.node(tree.selected())?;
        if node.is_expandable() {
            Some(node.path.clone())
        } else {
            node.path.parent().map(Path::to_path_buf)
        }
    }

    /// The selected entry, or `None` if it is the root (which must not be renamed or
    /// deleted from inside the explorer).
    fn selected_entry(&self) -> Option<(PathBuf, bool)> {
        let tree = &self.explorer.as_ref()?.tree;
        let selected = tree.selected();
        if selected == tree.root() {
            return None;
        }
        let node = tree.node(selected)?;
        Some((node.path.clone(), node.is_expandable()))
    }

    fn prompt_new(&mut self, directory: bool) {
        if self.focus != Pane::Project {
            return;
        }
        let Some(parent) = self.target_dir() else {
            self.notification = Some("open a workspace first".into());
            return;
        };
        let kind = if directory {
            PromptKind::NewDir { parent: parent.clone() }
        } else {
            PromptKind::NewFile { parent: parent.clone() }
        };
        let what = if directory { "folder" } else { "file" };
        self.overlays.push(Overlay::Prompt(Prompt {
            title: format!("New {what} in {}", display_name(&parent)),
            input: String::new(),
            kind,
        }));
    }

    fn prompt_rename(&mut self) {
        if self.focus != Pane::Project {
            return;
        }
        let Some((target, _)) = self.selected_entry() else {
            self.notification = Some("select an entry to rename".into());
            return;
        };
        self.overlays.push(Overlay::Prompt(Prompt {
            title: format!("Rename {}", display_name(&target)),
            // Pre-fill with the current name so a small edit is a small edit.
            input: display_name(&target),
            kind: PromptKind::Rename { target },
        }));
    }

    fn prompt_delete(&mut self) {
        if self.focus != Pane::Project {
            return;
        }
        let Some((target, is_dir)) = self.selected_entry() else {
            self.notification = Some("select an entry to delete".into());
            return;
        };
        let what = if is_dir { "directory and everything in it" } else { "file" };
        self.overlays.push(Overlay::Prompt(Prompt {
            title: format!("Delete {what}: {}?  (Enter to confirm)", display_name(&target)),
            input: String::new(),
            kind: PromptKind::ConfirmDelete { target, is_dir },
        }));
    }

    /// Turn a confirmed prompt into filesystem work. Rejects names that would escape the
    /// directory they were asked for — a `../` in a rename must not silently move a file
    /// out of the workspace.
    pub fn confirm_prompt(&mut self, prompt: Prompt) {
        let name = prompt.input.trim().to_string();

        if prompt.kind == PromptKind::Find {
            if name.is_empty() {
                self.find = None;
                if let Some(buffer) = self.active_buffer_mut() {
                    buffer.decorations_mut().clear_matches();
                }
                return;
            }
            self.run_find(name);
            return;
        }
        if prompt.kind == PromptKind::Replace {
            self.replace_all(prompt.input.clone());
            return;
        }

        if let PromptKind::ConfirmCloseBuffer { buffer } = prompt.kind {
            self.discard_buffer(buffer);
            return;
        }

        if let PromptKind::ConfirmCloseTerminal { terminal } = prompt.kind {
            self.remove_terminal(terminal, true);
            return;
        }

        if prompt.kind == PromptKind::TerminalRun {
            if name.is_empty() {
                self.notification = Some("command cannot be empty".into());
                return;
            }
            let spec = termesh_platform::human_command(prompt.input, &self.terminal_cwd());
            self.create_terminal(spec, TerminalOwner::HumanCommand);
            return;
        }

        if prompt.kind == PromptKind::GitCommit {
            if name.is_empty() {
                self.notification = Some("commit message cannot be empty".into());
                return;
            }
            self.queue_git_operation(termesh_core::GitOperation::Commit { message: name });
            return;
        }

        if prompt.kind == PromptKind::LspRename {
            if name.is_empty() {
                self.notification = Some("new symbol name cannot be empty".into());
                return;
            }
            self.request_lsp_rename(name);
            return;
        }

        // An agent turn is free text, not a filename, so it skips the path checks below.
        if let PromptKind::AgentPrompt { session } = prompt.kind {
            if name.is_empty() {
                self.notification = Some("nothing to ask".into());
                return;
            }
            match self.agent.as_ref() {
                Some(_) => self.send_prompt(session, name),
                // No session yet: open one and hold the question until it lands.
                None => {
                    self.pending_prompt = Some(name);
                    self.new_agent_session();
                    self.notification = Some("starting an agent session\u{2026}".into());
                }
            }
            return;
        }

        if prompt.takes_input() {
            if name.is_empty() {
                self.notification = Some("name cannot be empty".into());
                return;
            }
            if name.contains('/') || name.contains('\\') || name == ".." || name == "." {
                self.notification = Some(format!("invalid name: {name}"));
                return;
            }
        }

        let request = match prompt.kind {
            PromptKind::NewFile { parent } => FsRequest::CreateFile(parent.join(name)),
            PromptKind::NewDir { parent } => FsRequest::CreateDir(parent.join(name)),
            PromptKind::Rename { target } => {
                let Some(parent) = target.parent() else { return };
                FsRequest::Rename { from: target.clone(), to: parent.join(name) }
            }
            PromptKind::ConfirmDelete { target, is_dir } => {
                FsRequest::Remove { path: target, recursive: is_dir }
            }
            // Handled above: neither is filesystem work.
            PromptKind::AgentPrompt { .. }
            | PromptKind::ConfirmCloseBuffer { .. }
            | PromptKind::TerminalRun
            | PromptKind::GitCommit
            | PromptKind::LspRename
            | PromptKind::ConfirmCloseTerminal { .. }
            | PromptKind::Find
            | PromptKind::Replace => return,
        };
        self.outbox.push(request);
    }

    /// Enter on a directory expands it; on a file it opens it in the editor.
    fn explorer_toggle(&mut self) {
        let Some(tree) = self.explorer_focused() else { return };
        let selected = tree.selected();

        let file = tree.node(selected).filter(|n| !n.is_expandable()).map(|n| n.path.clone());
        if let Some(path) = file {
            self.open_file(path);
            return;
        }
        if let Some(path) = tree.toggle(selected) {
            self.outbox.push(FsRequest::ReadDir { id: selected, path });
        }
    }

    pub fn overlay_active(&self) -> bool {
        !self.overlays.is_empty()
    }

    #[cfg(test)]
    pub fn help_rows(&self) -> Vec<HelpRow> {
        self.overlays
            .iter()
            .rev()
            .find_map(|overlay| match overlay {
                Overlay::Help(help) => Some(help.all_rows()),
                _ => None,
            })
            .unwrap_or_default()
    }

    pub fn is_first_run(&self) -> bool {
        self.first_run
    }

    pub fn set_prior_session_present(&mut self, present: bool) {
        if present {
            self.first_run = false;
        }
    }

    /// The single dispatch path for every command (ARCHITECTURE.md §3, §7.1).
    pub fn dispatch(&mut self, cmd: Command) {
        self.notification = None;
        self.dispatch_inner(cmd);
        // Any command may have changed a buffer, and both highlighting and review state
        // are derived from buffer text — so they refresh once, here, rather than at each
        // of the dozen call sites that can edit.
        self.sync_syntax();
        self.sync_proposals();
        self.sync_lsp_documents();
    }

    fn dispatch_inner(&mut self, cmd: Command) {
        match cmd {
            Command::Action(Action::FileOpen) => self.open_quick_open(),
            Command::Action(Action::WorkspaceSearch) => self.open_workspace_search(),
            Command::Action(Action::TaskRun) => self.open_task_picker(),
            Command::Action(Action::TaskCancel) => self.cancel_latest_task(),
            Command::Action(Action::ProblemsShow) => self.show_problems(),
            Command::Action(Action::ProblemsNext) => self.step_problem(true),
            Command::Action(Action::ProblemsPrevious) => self.step_problem(false),
            Command::Action(Action::GitShow) => self.show_git_status(),
            Command::Action(Action::GitStage) => self.stage_selected_git_row(),
            Command::Action(Action::GitUnstage) => self.unstage_selected_git_row(),
            Command::Action(Action::GitCommit) => self.prompt_git_commit(),
            Command::Action(Action::GitBranchCheckout) => self.request_git_branches(),
            Command::Action(Action::GitFetch) => {
                self.queue_git_operation(termesh_core::GitOperation::Fetch)
            }
            Command::Action(Action::GitPull) => {
                self.queue_git_operation(termesh_core::GitOperation::Pull)
            }
            Command::Action(Action::GitPush) => {
                self.queue_git_operation(termesh_core::GitOperation::Push)
            }
            Command::Action(Action::EditorGotoDefinition) => self.request_lsp_definition(),
            Command::Action(Action::LspHover) => self.request_lsp_hover(),
            Command::Action(Action::LspCompletion) => self.request_lsp_completion(),
            Command::Action(Action::LspReferences) => self.request_lsp_references(),
            Command::Action(Action::LspDocumentSymbols) => self.request_lsp_document_symbols(),
            Command::Action(Action::LspWorkspaceSymbols) => self.request_lsp_workspace_symbols(),
            Command::Action(Action::LspFormat) => self.format_document(),
            Command::Action(Action::LspRename) => self.prompt_lsp_rename(),
            Command::Action(Action::LspCodeAction) => self.request_lsp_code_actions(),
            Command::Action(Action::LspRestart) => self.restart_language_servers(),
            Command::Action(Action::FileNew) => self.prompt_new(false),
            Command::Action(Action::FolderNew) => self.prompt_new(true),
            Command::Action(Action::FileRename) => self.prompt_rename(),
            Command::Action(Action::FileDelete) => self.prompt_delete(),
            Command::Action(Action::FileSave) => self.save_active_buffer(),
            Command::Action(Action::WorkspaceRestoreDrafts) => self.accept_recovery_drafts(),
            Command::Action(Action::AgentSessionNew) => self.new_agent_session(),
            Command::Action(Action::AgentPrompt) => self.prompt_agent(),
            Command::Action(Action::AgentMode) => self.open_agent_modes(),
            Command::Action(Action::AgentProposalAccept) => self.resolve_proposal(true),
            Command::Action(Action::AgentProposalReject) => self.resolve_proposal(false),
            Command::Action(Action::TerminalFocus) => self.toggle_terminal_focus(),
            Command::Action(Action::FocusProject) => self.focus_pane(Pane::Project),
            Command::Action(Action::FocusEditor) => self.focus_pane(Pane::Editor),
            Command::Action(Action::FocusAgent) => self.focus_pane(Pane::Agent),
            Command::Action(Action::TerminalNew) => self.new_shell_terminal(),
            Command::Action(Action::TerminalRun) => {
                self.overlays.push(Overlay::Prompt(Prompt {
                    title: "Run in terminal".into(),
                    input: String::new(),
                    kind: PromptKind::TerminalRun,
                }));
            }
            Command::Action(Action::TerminalNext) => self.cycle_terminal(1),
            Command::Action(Action::TerminalPrevious) => self.cycle_terminal(-1),
            Command::Action(Action::TerminalRestart) => self.restart_terminal(),
            Command::Action(Action::TerminalClose) => self.close_terminal(),
            Command::Action(Action::TerminalCopyMode) => self.enter_terminal_copy_mode(),
            Command::Action(Action::HelpShow) => {
                self.overlays.push(Overlay::Help(HelpOverlay::open(
                    &self.registry,
                    &self.keymap,
                    self.focus,
                )));
            }
            Command::Action(Action::ConfigReload) => self.reload_config(),
            Command::AgentScrollUp => {
                self.agent_scroll = (self.agent_scroll + 3).min(self.agent_scroll_max);
            }
            Command::AgentScrollDown => self.agent_scroll = self.agent_scroll.saturating_sub(3),
            Command::AgentAllowOnce => {
                self.decide_permission(termesh_core::PermissionDecision::AllowOnce)
            }
            Command::AgentAllowAlways => {
                self.decide_permission(termesh_core::PermissionDecision::AllowAlways)
            }
            Command::AgentDeny => {
                self.decide_permission(termesh_core::PermissionDecision::RejectOnce)
            }
            Command::Action(a) => {
                // Features that aren't built yet report intent rather than failing silently.
                let gate =
                    if a.agent_needs_permission() { " (agent: permission-gated)" } else { "" };
                self.notification =
                    Some(format!("invoked {}{} — lands in a later phase", a.id(), gate));
            }
            // The Tab ring deliberately excludes the Terminal. A focused shell owns Tab
            // (ADR-0008 §3), so a Terminal in the ring could be entered but never left by
            // the same key — a one-way door. It is reached by its own chord instead, and
            // cycling from inside it resumes from wherever the user last was.
            Command::FocusNext => {
                self.focus = match self.focus {
                    Pane::Project => Pane::Editor,
                    Pane::Editor => Pane::Agent,
                    Pane::Agent => Pane::Project,
                    Pane::Terminal => self.previous_non_terminal_focus,
                };
            }
            Command::FocusPrev => {
                self.focus = match self.focus {
                    Pane::Project => Pane::Agent,
                    Pane::Editor => Pane::Project,
                    Pane::Agent => Pane::Editor,
                    Pane::Terminal => self.previous_non_terminal_focus,
                };
            }
            Command::GrowSidebar => self.layout.grow_sidebar(),
            Command::ShrinkSidebar => self.layout.shrink_sidebar(),
            Command::GrowBottom => self.layout.grow_bottom(),
            Command::ShrinkBottom => self.layout.shrink_bottom(),
            Command::OpenPalette => {
                let p = Palette::open(&self.registry, &self.keymap);
                self.overlays.push(Overlay::Palette(p));
            }
            Command::CloseOverlay => {
                self.overlays.pop();
            }
            Command::Quit => self.running = false,
            Command::ExplorerNext => {
                if let Some(t) = self.explorer_focused() {
                    t.select_next();
                }
            }
            Command::ExplorerPrev => {
                if let Some(t) = self.explorer_focused() {
                    t.select_prev();
                }
            }
            Command::ExplorerToggle => self.explorer_toggle(),
            Command::ExplorerCollapseOrParent => {
                if let Some(t) = self.explorer_focused() {
                    t.collapse_or_parent();
                }
            }

            // --- editor ---------------------------------------------------------
            //
            // No focus guards: the keymap only resolves these in `KeyContext::Editor`,
            // and the palette reaching them with a buffer open is correct.
            Command::EditorCursorLeft => self.with_buffer(Buffer::move_left),
            Command::EditorCursorRight => self.with_buffer(Buffer::move_right),
            Command::EditorCursorUp => self.with_buffer(|b| b.move_line(false)),
            Command::EditorCursorDown => self.with_buffer(|b| b.move_line(true)),
            Command::EditorLineStart => self.with_buffer(Buffer::move_line_start),
            Command::EditorLineEnd => self.with_buffer(Buffer::move_line_end),
            Command::EditorInsertNewline => {
                self.edit_active(|b| b.insert("\n", EditSource::Keyboard))
            }
            Command::EditorBackspace => self.edit_active(Buffer::delete_backward),
            Command::EditorDeleteForward => self.edit_active(Buffer::delete_forward),
            Command::EditorFind => self.open_find_prompt(PromptKind::Find, "Find"),
            Command::EditorReplace => {
                if self.find.is_none() {
                    self.notification = Some("find something first (Ctrl+F)".into());
                } else {
                    self.open_find_prompt(PromptKind::Replace, "Replace all with");
                }
            }
            Command::EditorFindNext => self.step_match(true),
            Command::EditorFindPrev => self.step_match(false),
            Command::EditorNextTab => self.cycle_tab(1),
            Command::EditorPrevTab => self.cycle_tab(-1),
            Command::EditorCloseTab => self.close_tab(),
            Command::EditorUndo => {
                if !self.with_buffer_bool(Buffer::undo) {
                    self.notification = Some("nothing to undo".into());
                }
            }
            Command::EditorRedo => {
                if !self.with_buffer_bool(Buffer::redo) {
                    self.notification = Some("nothing to redo".into());
                }
            }
            Command::TerminalCopyLeft => self.move_terminal_copy_selection(0, -1, false),
            Command::TerminalCopyRight => self.move_terminal_copy_selection(0, 1, false),
            Command::TerminalCopyUp => self.move_terminal_copy_selection(-1, 0, false),
            Command::TerminalCopyDown => self.move_terminal_copy_selection(1, 0, false),
            Command::TerminalCopyExtendLeft => self.move_terminal_copy_selection(0, -1, true),
            Command::TerminalCopyExtendRight => self.move_terminal_copy_selection(0, 1, true),
            Command::TerminalCopyExtendUp => self.move_terminal_copy_selection(-1, 0, true),
            Command::TerminalCopyExtendDown => self.move_terminal_copy_selection(1, 0, true),
            Command::TerminalCopyPageUp => self.page_terminal_copy_selection(-1),
            Command::TerminalCopyPageDown => self.page_terminal_copy_selection(1),
            Command::TerminalCopyConfirm => self.confirm_terminal_copy_mode(),
            Command::TerminalCopyCancel => self.cancel_terminal_copy_mode(),
            Command::TerminalScrollUp => self.scroll_terminal_page(1),
            Command::TerminalScrollDown => self.scroll_terminal_page(-1),
        }
    }

    fn open_find_prompt(&mut self, kind: PromptKind, title: &str) {
        if self.active_buffer().is_none() {
            self.notification = Some("no file open".into());
            return;
        }
        let input = match kind {
            // Pre-fill with the current query so refining a search is a small edit.
            PromptKind::Find => self.find.as_ref().map(|f| f.query.clone()).unwrap_or_default(),
            _ => String::new(),
        };
        self.overlays.push(Overlay::Prompt(Prompt { title: title.into(), input, kind }));
    }

    /// Run `query` over the active buffer and jump to the first hit after the cursor.
    fn run_find(&mut self, query: String) {
        let Some(buffer) = self.active_buffer() else { return };
        let matches =
            termesh_editor::find_all(buffer.text(), &query, termesh_editor::CaseMode::default());
        let cursor = buffer.selection().primary().head;
        let current = termesh_editor::search::next_from(&matches, cursor);

        let found = matches.len();
        self.find = Some(Find { query: query.clone(), matches, current });
        self.notification = Some(match found {
            0 => format!("no matches for '{query}'"),
            n => format!("{n} match(es) for '{query}'"),
        });
        self.focus_match();
    }

    /// Step to the next or previous match, wrapping.
    fn step_match(&mut self, forward: bool) {
        let Some(find) = self.find.as_ref() else {
            self.notification = Some("nothing to search for (Ctrl+F)".into());
            return;
        };
        if find.matches.is_empty() {
            self.notification = Some(format!("no matches for '{}'", find.query));
            return;
        }

        // Backwards navigation is anchored on the current match's start, not the cursor,
        // or a cursor sitting inside a match would find that same match again.
        let from = match (forward, find.current) {
            (true, Some(i)) => find.matches[i].1,
            (false, Some(i)) => find.matches[i].0,
            (_, None) => 0,
        };
        let next = if forward {
            termesh_editor::search::next_from(&find.matches, from)
        } else {
            termesh_editor::search::prev_from(&find.matches, from)
        };
        if let Some(find) = self.find.as_mut() {
            find.current = next;
        }
        self.focus_match();
    }

    /// Put the cursor on the current match and paint the rest.
    fn focus_match(&mut self) {
        let Some(find) = self.find.clone() else { return };
        let height = self.editor_height;
        let Some(buffer) = self.active_buffer_mut() else { return };

        // Matches are derived data, so they use the same decoration path syntax and
        // diagnostics will — cleared wholesale and repainted rather than patched.
        buffer.decorations_mut().clear_matches();
        for (i, (start, end)) in find.matches.iter().enumerate() {
            buffer.decorations_mut().push(Decoration::new(
                *start,
                *end,
                DecorationClass::Match { current: Some(i) == find.current },
            ));
        }

        if let Some(i) = find.current {
            let (start, end) = find.matches[i];
            buffer.set_selection(termesh_editor::Selection::single(termesh_editor::Range::new(
                start, end,
            )));
            buffer.scroll_to_cursor(height);
        }
    }

    /// Replace every match in one transaction — one undo step for the whole operation.
    fn replace_all(&mut self, replacement: String) {
        let Some(find) = self.find.clone() else { return };
        if find.matches.is_empty() {
            self.notification = Some("nothing to replace".into());
            return;
        }

        let count = find.matches.len();
        let Some(buffer) = self.active_buffer_mut() else { return };

        let mut builder = termesh_editor::ChangeSet::builder(buffer.text().len_chars());
        let mut at = 0;
        for (start, end) in &find.matches {
            builder.retain(start - at);
            builder.delete(end - start);
            builder.insert(replacement.clone());
            at = *end;
        }
        let changes = builder.build();
        let tx = buffer.transaction(changes, EditSource::Replace);
        if let Err(e) = buffer.apply(&tx) {
            self.notification = Some(e.to_string());
            return;
        }

        // The old matches describe a document that no longer exists.
        self.find = None;
        if let Some(buffer) = self.active_buffer_mut() {
            buffer.decorations_mut().clear_matches();
        }
        self.notification = Some(format!("replaced {count} occurrence(s)"));
        self.sync_proposals();
    }

    /// Move to the next or previous open file, wrapping.
    fn cycle_tab(&mut self, step: isize) {
        if self.buffers.len() < 2 {
            return;
        }
        let Some(current) = self.active_buffer else { return };
        let count = self.buffers.len() as isize;
        let next = (current as isize + step).rem_euclid(count) as usize;
        self.active_buffer = Some(next);
        self.focus = Pane::Editor;
    }

    /// Close the active file, asking first if it has unsaved work.
    fn close_tab(&mut self) {
        let Some(buffer) = self.active_buffer() else {
            self.notification = Some("nothing to close".into());
            return;
        };
        if buffer.is_dirty() {
            let name = buffer.display_name();
            let id = buffer.id();
            self.overlays.push(Overlay::Prompt(Prompt {
                title: format!("{name} has unsaved changes — close anyway?  (Enter to confirm)"),
                input: String::new(),
                kind: PromptKind::ConfirmCloseBuffer { buffer: id },
            }));
            return;
        }
        self.discard_buffer(buffer.id());
    }

    /// Drop a buffer and pick a sensible neighbour to show.
    fn discard_buffer(&mut self, id: BufferId) {
        let Some(index) = self.buffers.iter().position(|b| b.id() == id) else { return };
        if let Some(path) = self.buffers[index].path().map(Path::to_path_buf) {
            self.close_lsp_document(&path);
        }
        self.buffers.remove(index);

        self.active_buffer = if self.buffers.is_empty() {
            None
        } else {
            // Land on the neighbour to the left, which is where the eye already is.
            Some(index.saturating_sub(1).min(self.buffers.len() - 1))
        };
    }

    /// Type `c` into the active buffer. Not a command: there is no finite set of
    /// "insert an x" actions, so literal text entry is the one input the keymap does not
    /// route (see `input::on_chord`).
    pub fn type_char(&mut self, c: char) {
        self.notification = None;
        self.edit_active(|b| b.insert(&c.to_string(), EditSource::Keyboard));
        self.sync_syntax();
        self.sync_proposals();
        self.sync_lsp_documents();
    }

    /// Run `f` on the active buffer, then scroll the least amount that keeps the cursor
    /// on screen. Every path that can move the cursor goes through here.
    fn with_buffer(&mut self, f: impl FnOnce(&mut Buffer)) {
        let height = self.editor_height;
        if let Some(b) = self.active_buffer_mut() {
            f(b);
            b.scroll_to_cursor(height);
        }
    }

    fn with_buffer_bool(&mut self, f: impl FnOnce(&mut Buffer) -> bool) -> bool {
        let height = self.editor_height;
        match self.active_buffer_mut() {
            Some(b) => {
                let moved = f(b);
                b.scroll_to_cursor(height);
                moved
            }
            None => false,
        }
    }
}

impl Default for Model {
    fn default() -> Self {
        Self::new()
    }
}

/// Turn a mutation outcome into the event the model expects.
fn is_java_build_file(path: &Path) -> bool {
    matches!(
        path.file_name().and_then(|name| name.to_str()),
        Some(
            "pom.xml"
                | "build.gradle"
                | "build.gradle.kts"
                | "settings.gradle"
                | "settings.gradle.kts"
        )
    )
}

fn mutation_event(outcome: termesh_core::FsResult<()>, path: std::path::PathBuf) -> FsEvent {
    match outcome {
        Ok(()) => FsEvent::Changed(vec![path]),
        Err(e) => FsEvent::MutationFailed(e),
    }
}

/// The last component of a path, for prompts and messages.
fn display_name(path: &Path) -> String {
    path.file_name().unwrap_or(path.as_os_str()).to_string_lossy().into_owned()
}

fn preview_window(contents: &str, line: usize, context: usize) -> (usize, String) {
    let lines: Vec<&str> = contents.split_inclusive('\n').collect();
    let target = line.saturating_sub(1).min(lines.len().saturating_sub(1));
    let start = target.saturating_sub(context);
    let end = (target + context + 1).min(lines.len());
    (start + 1, lines[start..end].concat())
}

fn normalize_problem(
    cwd: &Path,
    mut problem: termesh_core::Problem,
) -> Option<termesh_core::Problem> {
    use std::path::Component;
    if problem.path.components().any(|component| component == Component::ParentDir) {
        return None;
    }
    if problem.path.is_relative() {
        problem.path = cwd.join(problem.path);
    }
    Some(problem)
}

fn normalize_problem_message(message: &str) -> String {
    message.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn executable_name(program: &str) -> String {
    Path::new(program)
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new(program))
        .to_string_lossy()
        .into_owned()
}

fn flatten_document_symbols(
    symbols: &[termesh_core::DocumentSymbol],
    path: &Path,
    depth: usize,
    rows: &mut Vec<SymbolRow>,
) {
    for symbol in symbols {
        rows.push(SymbolRow {
            label: symbol.name.clone(),
            detail: symbol.detail.clone(),
            depth,
            location: termesh_core::Location { path: path.to_path_buf(), range: symbol.range },
        });
        flatten_document_symbols(&symbol.children, path, depth + 1, rows);
    }
}

fn diagnostic_severity_rank(severity: termesh_core::DiagnosticSeverity) -> u8 {
    match severity {
        termesh_core::DiagnosticSeverity::Error => 0,
        termesh_core::DiagnosticSeverity::Warning => 1,
        termesh_core::DiagnosticSeverity::Info => 2,
        termesh_core::DiagnosticSeverity::Hint => 3,
    }
}

fn diagnostic_severity_name(severity: termesh_core::DiagnosticSeverity) -> &'static str {
    match severity {
        termesh_core::DiagnosticSeverity::Error => "error",
        termesh_core::DiagnosticSeverity::Warning => "warning",
        termesh_core::DiagnosticSeverity::Info => "info",
        termesh_core::DiagnosticSeverity::Hint => "hint",
    }
}

fn append_symbol_outline(
    context: &mut BoundedContext,
    symbols: &[DocumentSymbol],
    depth: usize,
) -> bool {
    for symbol in symbols {
        let kind = match symbol.kind {
            termesh_core::SymbolKind::File => "file",
            termesh_core::SymbolKind::Module => "mod",
            termesh_core::SymbolKind::Struct => "struct",
            termesh_core::SymbolKind::Enum => "enum",
            termesh_core::SymbolKind::Trait => "trait",
            termesh_core::SymbolKind::Function => "fn",
            termesh_core::SymbolKind::Method => "fn",
            termesh_core::SymbolKind::Field => "field",
            termesh_core::SymbolKind::Constant => "const",
            termesh_core::SymbolKind::Variable => "let",
            termesh_core::SymbolKind::TypeAlias => "type",
            termesh_core::SymbolKind::Macro => "macro",
            termesh_core::SymbolKind::Other => "symbol",
        };
        if !context.line(&format!("{}{} {}", "  ".repeat(depth), kind, symbol.name)) {
            return false;
        }
        if !append_symbol_outline(context, &symbol.children, depth + 1) {
            return false;
        }
    }
    true
}

fn build_lsp_changes(
    buffer: &Buffer,
    edits: &[TextEdit],
) -> Result<termesh_editor::ChangeSet, String> {
    let mut converted: Vec<_> = edits
        .iter()
        .map(|edit| {
            let start = termesh_editor::position::offset_from_utf16(
                buffer.text(),
                edit.range.start.line,
                edit.range.start.character,
            );
            let end = termesh_editor::position::offset_from_utf16(
                buffer.text(),
                edit.range.end.line,
                edit.range.end.character,
            );
            (start, end, edit.new_text.clone())
        })
        .collect();
    if converted.iter().any(|(start, end, _)| start > end) {
        return Err("Language server returned an edit with a reversed range".into());
    }
    converted.sort_by_key(|(start, end, _)| (*start, *end));
    if converted.windows(2).any(|pair| {
        let (left_start, left_end, _) = &pair[0];
        let (right_start, right_end, _) = &pair[1];
        left_end > right_start
            || (left_start == left_end && right_start == right_end && left_start == right_start)
    }) {
        return Err("Language server returned overlapping edits".into());
    }

    let mut builder = termesh_editor::ChangeSet::builder(buffer.text().len_chars());
    let mut at = 0;
    for (start, end, new_text) in converted {
        builder.retain(start - at);
        builder.delete(end - start);
        builder.insert(new_text);
        at = end;
    }
    Ok(builder.build())
}

/// Case-sensitive subsequence match (`gt` matches "Go To ...").
fn subsequence(needle: &str, haystack: &str) -> bool {
    let mut chars = haystack.chars();
    needle.chars().all(|n| chars.any(|h| h == n))
}
