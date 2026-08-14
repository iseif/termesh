//! Protocol-neutral language-server state and session messages (ADR-0011).
//!
//! CLI and wire details stay in `termesh-lsp`; these types live here because the
//! application message bus and single-owner model must carry them without depending
//! on a backend.

use std::path::PathBuf;

use crate::{LspRequestId, LspServerId};

/// A position in a document. `character` counts **UTF-16 code units**, which is what the
/// protocol speaks. The editor speaks char offsets; `termesh_editor::position` converts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct TextPosition {
    pub line: u32,
    pub character: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextRange {
    pub start: TextPosition,
    pub end: TextPosition,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum DiagnosticSeverity {
    Error,
    Warning,
    Info,
    Hint,
}

/// Which producer reported this. Cargo and a language server surface the same rustc
/// diagnostics, so the problems panel needs to tell them apart to deduplicate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticOrigin {
    LanguageServer,
    Task,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub path: PathBuf,
    pub range: TextRange,
    pub severity: DiagnosticSeverity,
    pub origin: DiagnosticOrigin,
    pub source: String,
    pub code: Option<String>,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Location {
    pub path: PathBuf,
    pub range: TextRange,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextEdit {
    pub path: PathBuf,
    pub range: TextRange,
    pub new_text: String,
}

/// A set of edits across one or more files. `version` is the wire version the server
/// authored against, when it supplied one.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkspaceEdit {
    pub edits: Vec<TextEdit>,
    pub versions: Vec<(PathBuf, u64)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymbolKind {
    File,
    Module,
    Struct,
    Enum,
    Trait,
    Function,
    Method,
    Field,
    Constant,
    Variable,
    TypeAlias,
    Macro,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocumentSymbol {
    pub name: String,
    pub kind: SymbolKind,
    pub detail: Option<String>,
    pub range: TextRange,
    pub children: Vec<DocumentSymbol>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolLocation {
    pub name: String,
    pub kind: SymbolKind,
    pub container: Option<String>,
    pub location: Location,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionItem {
    pub label: String,
    pub detail: Option<String>,
    pub kind: SymbolKind,
    /// What to insert. Never derived from `label` at the call site.
    pub insert_text: String,
    pub edit: Option<TextEdit>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HoverText {
    pub text: String,
    pub range: Option<TextRange>,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeAction {
    pub title: String,
    pub kind: Option<String>,
    pub edit: Option<WorkspaceEdit>,
}

/// One replaced span, or a whole-document replacement when `range` is `None`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextChange {
    pub range: Option<TextRange>,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchedFileChange {
    Created(PathBuf),
    Changed(PathBuf),
    Deleted(PathBuf),
}

/// Every response-bearing variant carries its correlation id first, so the model's
/// `active_*` guard can drop a superseded reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LspRequest {
    Start {
        server: LspServerId,
        root: PathBuf,
        command: Vec<String>,
        language: String,
        /// Raw JSON for `initializationOptions`, parsed at the wire boundary.
        ///
        /// A string rather than a `serde_json::Value` because `core` has zero
        /// dependencies. Rust needs none of this; Eclipse JDT LS and pyright do not
        /// start usefully without it, and carrying the field now keeps a later
        /// language a recipe change instead of a `protocol.rs` change.
        initialization_options: Option<String>,
    },
    DidOpen {
        path: PathBuf,
        language_id: String,
        version: u64,
        text: String,
    },
    DidChange {
        path: PathBuf,
        version: u64,
        change: TextChange,
    },
    DidSave {
        path: PathBuf,
    },
    DidClose {
        path: PathBuf,
    },
    WatchedFilesChanged {
        changes: Vec<WatchedFileChange>,
    },
    /// Ask a server to refresh project metadata affected by configuration files.
    /// The protocol translator chooses the vendor method; core stays neutral.
    ReloadProject {
        paths: Vec<PathBuf>,
    },
    Definition {
        id: LspRequestId,
        path: PathBuf,
        position: TextPosition,
    },
    Hover {
        id: LspRequestId,
        path: PathBuf,
        position: TextPosition,
    },
    Completion {
        id: LspRequestId,
        path: PathBuf,
        position: TextPosition,
    },
    References {
        id: LspRequestId,
        path: PathBuf,
        position: TextPosition,
    },
    DocumentSymbols {
        id: LspRequestId,
        path: PathBuf,
    },
    WorkspaceSymbols {
        id: LspRequestId,
        query: String,
    },
    Rename {
        id: LspRequestId,
        path: PathBuf,
        position: TextPosition,
        new_name: String,
    },
    CodeActions {
        id: LspRequestId,
        path: PathBuf,
        range: TextRange,
    },
    Formatting {
        id: LspRequestId,
        path: PathBuf,
    },
    Cancel {
        id: LspRequestId,
    },
    Shutdown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LspFailureKind {
    NotInstalled,
    Handshake,
    Transport,
    Server,
    Unsupported,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LspFailure {
    pub kind: LspFailureKind,
    pub message: String,
}

pub type LspResult<T> = Result<T, LspFailure>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LspEvent {
    Started,
    Ready,
    Indexing { message: String, percent: Option<u8> },
    Diagnostics { path: PathBuf, version: Option<u64>, items: Vec<Diagnostic> },
    Definition { id: LspRequestId, locations: Vec<Location> },
    Hover { id: LspRequestId, hover: Option<HoverText> },
    Completion { id: LspRequestId, items: Vec<CompletionItem> },
    References { id: LspRequestId, locations: Vec<Location> },
    DocumentSymbols { id: LspRequestId, symbols: Vec<DocumentSymbol> },
    WorkspaceSymbols { id: LspRequestId, symbols: Vec<SymbolLocation> },
    Rename { id: LspRequestId, edit: WorkspaceEdit },
    CodeActions { id: LspRequestId, actions: Vec<CodeAction> },
    Formatting { id: LspRequestId, edits: Vec<TextEdit> },
    Failed { id: Option<LspRequestId>, failure: LspFailure },
    Unavailable { message: String },
    Exited { code: Option<i32> },
}
