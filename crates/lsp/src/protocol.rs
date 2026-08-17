//! Translation between protocol-neutral core requests/events and LSP JSON fields.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde_json::{json, Value};
use termesh_core::{
    CodeAction, CompletionItem, Diagnostic, DiagnosticOrigin, DiagnosticSeverity, DocumentSymbol,
    HoverText, Location, LspEvent, LspFailure, LspFailureKind, LspRequest, LspRequestId, LspResult,
    SymbolKind, SymbolLocation, TextEdit, TextPosition, TextRange, WatchedFileChange,
    WorkspaceEdit,
};

use crate::{Message, RequestIds};

const MAX_DIAGNOSTICS: usize = 500;
const MAX_LOCATIONS: usize = 500;
const MAX_COMPLETIONS: usize = 200;
const MAX_SYMBOLS: usize = 500;
const MAX_CODE_ACTIONS: usize = 100;
const MAX_HOVER_CHARS: usize = 32 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    New,
    Initializing(u64),
    Ready,
}

#[derive(Debug)]
enum PendingKind {
    Definition,
    Hover,
    Completion { path: PathBuf },
    References,
    DocumentSymbols,
    WorkspaceSymbols,
    Rename,
    CodeActions,
    Formatting { path: PathBuf },
    Shutdown,
}

#[derive(Debug)]
struct Pending {
    external_id: Option<LspRequestId>,
    kind: PendingKind,
}

#[derive(Debug)]
pub struct Translator {
    state: State,
    ids: RequestIds,
    pending: HashMap<u64, Pending>,
    external_to_wire: HashMap<LspRequestId, u64>,
    cancelled: HashSet<u64>,
    queued: Vec<LspRequest>,
    /// Progress tokens the server has begun and not yet ended.
    ///
    /// `$/progress` is a stream of begin/report/end per token, and rust-analyzer runs
    /// several at once — fetching metadata, scanning roots, indexing. Treating every
    /// notification as "still working" left the status bar reporting indexing forever,
    /// because nothing was listening for the end.
    active_progress: HashSet<String>,
}

impl Default for Translator {
    fn default() -> Self {
        Self {
            state: State::New,
            ids: RequestIds::default(),
            pending: HashMap::new(),
            external_to_wire: HashMap::new(),
            cancelled: HashSet::new(),
            active_progress: HashSet::new(),
            queued: Vec::new(),
        }
    }
}

impl Translator {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn initialize(&mut self, root: PathBuf) -> Message {
        self.initialize_with(root, None)
            .expect("no initializationOptions is always a valid handshake")
    }

    pub fn initialize_with(&mut self, root: PathBuf, options: Option<&str>) -> LspResult<Message> {
        let options = options
            .map(|raw| {
                serde_json::from_str(raw).map_err(|error| LspFailure {
                    kind: LspFailureKind::Handshake,
                    message: format!("invalid initializationOptions: {error}"),
                })
            })
            .transpose()?;

        let id = self.ids.allocate();
        self.state = State::Initializing(id);
        let mut params = json!({
            "processId": Value::Null,
            "rootUri": path_to_uri(&root),
            "capabilities": {
                "window": {"workDoneProgress": true},
                "workspace": {
                    "workspaceFolders": true,
                    "didChangeWatchedFiles": {"dynamicRegistration": true},
                    "symbol": {"dynamicRegistration": false}
                },
                "textDocument": {
                    "synchronization": {
                        "dynamicRegistration": false,
                        "didSave": true,
                        "willSave": false,
                        "willSaveWaitUntil": false
                    },
                    "definition": {"dynamicRegistration": false, "linkSupport": true},
                    "hover": {"dynamicRegistration": false, "contentFormat": ["markdown", "plaintext"]},
                    "completion": {"dynamicRegistration": false},
                    "references": {"dynamicRegistration": false},
                    "documentSymbol": {"dynamicRegistration": false, "hierarchicalDocumentSymbolSupport": true},
                    "rename": {"dynamicRegistration": false, "prepareSupport": false},
                    "codeAction": {"dynamicRegistration": false},
                    "formatting": {"dynamicRegistration": false}
                }
            },
            "workspaceFolders": [{"uri": path_to_uri(&root), "name": workspace_name(&root)}]
        });
        if let Some(options) = options {
            params["initializationOptions"] = options;
        }
        Ok(Message::Request { id, method: "initialize".into(), params })
    }

    pub fn outgoing(&mut self, request: LspRequest) -> Vec<Message> {
        if self.state != State::Ready {
            self.queued.push(request);
            return Vec::new();
        }
        if let LspRequest::ReloadProject { paths } = request {
            // Eclipse declares the singular extension as a notification taking a
            // TextDocumentIdentifier. Expand batches here so core stays vendor-neutral.
            return paths
                .into_iter()
                .map(|path| {
                    notification(
                        "java/projectConfigurationUpdate",
                        json!({"uri":path_to_uri(&path)}),
                    )
                })
                .collect();
        }
        self.encode_request(request).into_iter().collect()
    }

    pub fn incoming(&mut self, message: Message) -> (Vec<LspEvent>, Vec<Message>) {
        match message {
            Message::Request { id, method, params } => {
                (Vec::new(), vec![answer_client_request(id, &method, &params)])
            }
            Message::Notification { method, params } => {
                (self.notification_events(&method, &params), Vec::new())
            }
            Message::Response { id, result } => self.response(id, Ok(result)),
            Message::Error { id, code, message } => self.response(id, Err((code, message))),
        }
    }

    fn response(
        &mut self,
        id: u64,
        response: Result<Value, (i64, String)>,
    ) -> (Vec<LspEvent>, Vec<Message>) {
        if self.state == State::Initializing(id) {
            return match response {
                Ok(_) => {
                    self.state = State::Ready;
                    let mut messages = vec![Message::Notification {
                        method: "initialized".into(),
                        params: json!({}),
                    }];
                    for request in std::mem::take(&mut self.queued) {
                        messages.extend(self.outgoing(request));
                    }
                    (vec![LspEvent::Ready], messages)
                }
                Err((code, message)) => {
                    self.state = State::New;
                    (
                        vec![LspEvent::Failed {
                            id: None,
                            failure: LspFailure {
                                kind: LspFailureKind::Handshake,
                                message: format!("initialize failed ({code}): {message}"),
                            },
                        }],
                        Vec::new(),
                    )
                }
            };
        }

        let Some(pending) = self.pending.remove(&id) else {
            return (Vec::new(), Vec::new());
        };
        if let Some(external_id) = pending.external_id {
            self.external_to_wire.remove(&external_id);
        }
        if self.cancelled.remove(&id) {
            return (Vec::new(), Vec::new());
        }
        let Some(external_id) = pending.external_id else {
            return (Vec::new(), Vec::new());
        };

        match response {
            Ok(result) => (
                response_event(external_id, pending.kind, result).into_iter().collect(),
                Vec::new(),
            ),
            Err((code, message)) => (
                vec![LspEvent::Failed {
                    id: Some(external_id),
                    failure: LspFailure {
                        kind: LspFailureKind::Server,
                        message: format!("language request failed ({code}): {message}"),
                    },
                }],
                Vec::new(),
            ),
        }
    }

    fn encode_request(&mut self, request: LspRequest) -> Option<Message> {
        match request {
            LspRequest::Start { .. } => None,
            LspRequest::DidOpen { path, language_id, version, text } => Some(notification(
                "textDocument/didOpen",
                json!({
                    "textDocument": {"uri":path_to_uri(&path),"languageId":language_id,
                                     "version":version,"text":text}
                }),
            )),
            LspRequest::DidChange { path, version, change } => {
                let mut content = json!({"text":change.text});
                if let Some(range) = change.range {
                    content["range"] = range_json(range);
                }
                Some(notification(
                    "textDocument/didChange",
                    json!({
                        "textDocument":{"uri":path_to_uri(&path),"version":version},
                        "contentChanges":[content]
                    }),
                ))
            }
            LspRequest::DidSave { path } => Some(notification(
                "textDocument/didSave",
                json!({"textDocument":{"uri":path_to_uri(&path)}}),
            )),
            LspRequest::DidClose { path } => Some(notification(
                "textDocument/didClose",
                json!({"textDocument":{"uri":path_to_uri(&path)}}),
            )),
            LspRequest::WatchedFilesChanged { changes } => {
                let changes: Vec<Value> = changes
                    .into_iter()
                    .map(|change| match change {
                        WatchedFileChange::Created(path) => {
                            json!({"uri":path_to_uri(&path),"type":1})
                        }
                        WatchedFileChange::Changed(path) => {
                            json!({"uri":path_to_uri(&path),"type":2})
                        }
                        WatchedFileChange::Deleted(path) => {
                            json!({"uri":path_to_uri(&path),"type":3})
                        }
                    })
                    .collect();
                Some(notification("workspace/didChangeWatchedFiles", json!({"changes":changes})))
            }
            LspRequest::ReloadProject { .. } => {
                unreachable!("project reload batches are expanded by outgoing")
            }
            LspRequest::Definition { id, path, position } => Some(self.tracked_request(
                id,
                PendingKind::Definition,
                "textDocument/definition",
                document_position(&path, position),
            )),
            LspRequest::Hover { id, path, position } => Some(self.tracked_request(
                id,
                PendingKind::Hover,
                "textDocument/hover",
                document_position(&path, position),
            )),
            LspRequest::Completion { id, path, position } => Some(self.tracked_request(
                id,
                PendingKind::Completion { path: path.clone() },
                "textDocument/completion",
                document_position(&path, position),
            )),
            LspRequest::References { id, path, position } => {
                let mut params = document_position(&path, position);
                params["context"] = json!({"includeDeclaration":true});
                Some(self.tracked_request(
                    id,
                    PendingKind::References,
                    "textDocument/references",
                    params,
                ))
            }
            LspRequest::DocumentSymbols { id, path } => Some(self.tracked_request(
                id,
                PendingKind::DocumentSymbols,
                "textDocument/documentSymbol",
                json!({"textDocument":{"uri":path_to_uri(&path)}}),
            )),
            LspRequest::WorkspaceSymbols { id, query } => Some(self.tracked_request(
                id,
                PendingKind::WorkspaceSymbols,
                "workspace/symbol",
                json!({"query":query}),
            )),
            LspRequest::Rename { id, path, position, new_name } => {
                let mut params = document_position(&path, position);
                params["newName"] = Value::String(new_name);
                Some(self.tracked_request(id, PendingKind::Rename, "textDocument/rename", params))
            }
            LspRequest::CodeActions { id, path, range } => Some(self.tracked_request(
                id,
                PendingKind::CodeActions,
                "textDocument/codeAction",
                json!({"textDocument":{"uri":path_to_uri(&path)},"range":range_json(range),
                       "context":{"diagnostics":[]}}),
            )),
            LspRequest::Formatting { id, path } => Some(self.tracked_request(
                id,
                PendingKind::Formatting { path: path.clone() },
                "textDocument/formatting",
                json!({"textDocument":{"uri":path_to_uri(&path)},
                       "options":{"tabSize":4,"insertSpaces":true}}),
            )),
            LspRequest::Cancel { id } => {
                let wire_id = *self.external_to_wire.get(&id)?;
                self.cancelled.insert(wire_id);
                Some(notification("$/cancelRequest", json!({"id":wire_id})))
            }
            LspRequest::Shutdown => {
                let id = self.ids.allocate();
                self.pending.insert(id, Pending { external_id: None, kind: PendingKind::Shutdown });
                Some(Message::Request { id, method: "shutdown".into(), params: Value::Null })
            }
        }
    }

    fn tracked_request(
        &mut self,
        external_id: LspRequestId,
        kind: PendingKind,
        method: &str,
        params: Value,
    ) -> Message {
        let id = self.ids.allocate();
        self.pending.insert(id, Pending { external_id: Some(external_id), kind });
        self.external_to_wire.insert(external_id, id);
        Message::Request { id, method: method.into(), params }
    }
}

fn workspace_name(root: &Path) -> String {
    root.file_name().unwrap_or(root.as_os_str()).to_string_lossy().into_owned()
}

fn notification(method: &str, params: Value) -> Message {
    Message::Notification { method: method.into(), params }
}

fn document_position(path: &Path, position: TextPosition) -> Value {
    json!({"textDocument":{"uri":path_to_uri(path)},"position":position_json(position)})
}

fn position_json(position: TextPosition) -> Value {
    json!({"line":position.line,"character":position.character})
}

fn range_json(range: TextRange) -> Value {
    json!({"start":position_json(range.start),"end":position_json(range.end)})
}

fn answer_client_request(id: u64, method: &str, params: &Value) -> Message {
    match method {
        "client/registerCapability" | "window/workDoneProgress/create" | "telemetry/event" => {
            Message::Response { id, result: Value::Null }
        }
        "workspace/configuration" => {
            let count = params.get("items").and_then(Value::as_array).map_or(0, Vec::len);
            Message::Response { id, result: Value::Array(vec![Value::Null; count]) }
        }
        _ => Message::Error { id, code: -32601, message: "Method not found".into() },
    }
}

impl Translator {
    fn notification_events(&mut self, method: &str, params: &Value) -> Vec<LspEvent> {
        match method {
            "textDocument/publishDiagnostics" => parse_diagnostics(params).into_iter().collect(),
            "$/progress" => self.progress_events(params),
            "language/status" => vec![parse_jdt_language_status(params)],
            _ => Vec::new(),
        }
    }

    /// Turn one `$/progress` notification into a load-state change.
    ///
    /// Only the *last* token ending means the server is done, so an `end` while other work
    /// is outstanding reports nothing and leaves the existing status alone.
    fn progress_events(&mut self, params: &Value) -> Vec<LspEvent> {
        let Some(indexing) = parse_progress(params) else { return Vec::new() };

        // A server that sends no token cannot be tracked; report it as work in progress,
        // which is what this did for every notification before.
        let Some(token) = progress_token(params) else { return vec![indexing] };

        match params.get("value").and_then(|v| v.get("kind")).and_then(Value::as_str) {
            Some("end") => {
                self.active_progress.remove(&token);
                if self.active_progress.is_empty() {
                    vec![LspEvent::Ready]
                } else {
                    Vec::new()
                }
            }
            _ => {
                self.active_progress.insert(token);
                vec![indexing]
            }
        }
    }
}

/// The token identifying one unit of server-side work. It is a string or a number on the
/// wire; either is fine as long as begin and end agree, so both become a string.
fn progress_token(params: &Value) -> Option<String> {
    match params.get("token")? {
        Value::String(token) => Some(token.clone()),
        Value::Number(token) => Some(token.to_string()),
        _ => None,
    }
}

fn parse_diagnostics(params: &Value) -> Option<LspEvent> {
    let path = uri_to_path(params.get("uri")?.as_str()?)?;
    let version = params.get("version").and_then(Value::as_u64);
    let items = params
        .get("diagnostics")?
        .as_array()?
        .iter()
        .take(MAX_DIAGNOSTICS)
        .filter_map(|item| {
            Some(Diagnostic {
                path: path.clone(),
                range: text_range(item.get("range")?)?,
                severity: diagnostic_severity(item.get("severity").and_then(Value::as_u64)),
                origin: DiagnosticOrigin::LanguageServer,
                source: item
                    .get("source")
                    .and_then(Value::as_str)
                    .unwrap_or("language server")
                    .into(),
                code: item.get("code").and_then(code_string),
                message: item.get("message").and_then(Value::as_str).unwrap_or_default().into(),
            })
        })
        .collect();
    Some(LspEvent::Diagnostics { path, version, items })
}

fn diagnostic_severity(severity: Option<u64>) -> DiagnosticSeverity {
    match severity {
        Some(1) => DiagnosticSeverity::Error,
        Some(2) => DiagnosticSeverity::Warning,
        Some(4) => DiagnosticSeverity::Hint,
        _ => DiagnosticSeverity::Info,
    }
}

fn code_string(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(str::to_string)
        .or_else(|| value.as_i64().map(|number| number.to_string()))
        .or_else(|| value.as_u64().map(|number| number.to_string()))
}

fn parse_progress(params: &Value) -> Option<LspEvent> {
    let value = params.get("value")?;
    let message = value
        .get("message")
        .and_then(Value::as_str)
        .or_else(|| value.get("title").and_then(Value::as_str))
        .unwrap_or("indexing")
        .to_string();
    let percent = value.get("percentage").and_then(Value::as_u64).map(|value| value.min(100) as u8);
    Some(LspEvent::Indexing { message, percent })
}

fn parse_jdt_language_status(params: &Value) -> LspEvent {
    // JDT LS has added status kinds across versions. Only the values that clearly
    // mean ready end indexing; every unknown value remains visible as progress
    // instead of making a cold import look hung.
    match params.get("type").and_then(Value::as_str) {
        Some("Started" | "ServiceReady") => LspEvent::Ready,
        _ => LspEvent::Indexing {
            message: params
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("indexing Java workspace")
                .to_string(),
            percent: None,
        },
    }
}

fn response_event(id: LspRequestId, kind: PendingKind, result: Value) -> Option<LspEvent> {
    match kind {
        PendingKind::Definition => Some(LspEvent::Definition { id, locations: locations(&result) }),
        PendingKind::Hover => Some(LspEvent::Hover { id, hover: hover(&result) }),
        PendingKind::Completion { path } => {
            Some(LspEvent::Completion { id, items: completion_items(&result, &path) })
        }
        PendingKind::References => Some(LspEvent::References { id, locations: locations(&result) }),
        PendingKind::DocumentSymbols => {
            Some(LspEvent::DocumentSymbols { id, symbols: document_symbols(&result) })
        }
        PendingKind::WorkspaceSymbols => {
            Some(LspEvent::WorkspaceSymbols { id, symbols: workspace_symbols(&result) })
        }
        PendingKind::Rename => Some(LspEvent::Rename { id, edit: workspace_edit(&result) }),
        PendingKind::CodeActions => {
            Some(LspEvent::CodeActions { id, actions: code_actions(&result) })
        }
        PendingKind::Formatting { path } => {
            Some(LspEvent::Formatting { id, edits: text_edits(&result, &path) })
        }
        PendingKind::Shutdown => None,
    }
}

fn text_position(value: &Value) -> Option<TextPosition> {
    Some(TextPosition {
        line: u32::try_from(value.get("line")?.as_u64()?).ok()?,
        character: u32::try_from(value.get("character")?.as_u64()?).ok()?,
    })
}

fn text_range(value: &Value) -> Option<TextRange> {
    Some(TextRange {
        start: text_position(value.get("start")?)?,
        end: text_position(value.get("end")?)?,
    })
}

fn location(value: &Value) -> Option<Location> {
    let uri = value.get("uri").or_else(|| value.get("targetUri"))?.as_str()?;
    let range = value
        .get("range")
        .or_else(|| value.get("targetSelectionRange"))
        .or_else(|| value.get("targetRange"))?;
    Some(Location { path: uri_to_path(uri)?, range: text_range(range)? })
}

fn locations(value: &Value) -> Vec<Location> {
    if value.is_null() {
        return Vec::new();
    }
    match value.as_array() {
        Some(values) => values.iter().take(MAX_LOCATIONS).filter_map(location).collect(),
        None => location(value).into_iter().collect(),
    }
}

fn hover(value: &Value) -> Option<HoverText> {
    if value.is_null() {
        return None;
    }
    let text = hover_contents(value.get("contents")?)?;
    let (text, truncated) = truncate_chars(&text, MAX_HOVER_CHARS);
    Some(HoverText { text, range: value.get("range").and_then(text_range), truncated })
}

fn hover_contents(value: &Value) -> Option<String> {
    if let Some(text) = value.as_str() {
        return Some(text.to_string());
    }
    if let Some(value) = value.get("value").and_then(Value::as_str) {
        return Some(value.to_string());
    }
    value
        .as_array()
        .map(|parts| parts.iter().filter_map(hover_contents).collect::<Vec<_>>().join("\n\n"))
}

fn truncate_chars(text: &str, limit: usize) -> (String, bool) {
    if text.chars().count() <= limit {
        return (text.to_string(), false);
    }
    (text.chars().take(limit).collect(), true)
}

fn completion_items(value: &Value, path: &Path) -> Vec<CompletionItem> {
    let values = value.as_array().or_else(|| value.get("items").and_then(Value::as_array));
    values
        .into_iter()
        .flatten()
        .take(MAX_COMPLETIONS)
        .filter_map(|item| {
            let label = item.get("label")?.as_str()?.to_string();
            Some(CompletionItem {
                detail: item.get("detail").and_then(Value::as_str).map(str::to_string),
                kind: completion_kind(item.get("kind").and_then(Value::as_u64)),
                insert_text: item
                    .get("insertText")
                    .and_then(Value::as_str)
                    .unwrap_or(&label)
                    .to_string(),
                edit: item.get("textEdit").and_then(|edit| text_edit(edit, path)),
                label,
            })
        })
        .collect()
}

fn text_edit(value: &Value, path: &Path) -> Option<TextEdit> {
    Some(TextEdit {
        path: path.to_path_buf(),
        range: text_range(value.get("range").or_else(|| value.get("replace"))?)?,
        new_text: value.get("newText")?.as_str()?.to_string(),
    })
}

fn text_edits(value: &Value, path: &Path) -> Vec<TextEdit> {
    value.as_array().into_iter().flatten().filter_map(|edit| text_edit(edit, path)).collect()
}

fn document_symbols(value: &Value) -> Vec<DocumentSymbol> {
    value.as_array().into_iter().flatten().take(MAX_SYMBOLS).filter_map(document_symbol).collect()
}

fn document_symbol(value: &Value) -> Option<DocumentSymbol> {
    let range = value
        .get("range")
        .or_else(|| value.get("location").and_then(|location| location.get("range")))?;
    let children = value
        .get("children")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .take(MAX_SYMBOLS)
        .filter_map(document_symbol)
        .collect();
    Some(DocumentSymbol {
        name: value.get("name")?.as_str()?.to_string(),
        kind: symbol_kind(value.get("kind").and_then(Value::as_u64)),
        detail: value
            .get("detail")
            .or_else(|| value.get("containerName"))
            .and_then(Value::as_str)
            .map(str::to_string),
        range: text_range(range)?,
        children,
    })
}

fn workspace_symbols(value: &Value) -> Vec<SymbolLocation> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .take(MAX_SYMBOLS)
        .filter_map(|symbol| {
            Some(SymbolLocation {
                name: symbol.get("name")?.as_str()?.to_string(),
                kind: symbol_kind(symbol.get("kind").and_then(Value::as_u64)),
                container: symbol.get("containerName").and_then(Value::as_str).map(str::to_string),
                location: location(symbol.get("location")?)?,
            })
        })
        .collect()
}

fn workspace_edit(value: &Value) -> WorkspaceEdit {
    let mut edit = WorkspaceEdit::default();
    if let Some(changes) = value.get("changes").and_then(Value::as_object) {
        for (uri, values) in changes {
            let Some(path) = uri_to_path(uri) else { continue };
            edit.edits.extend(text_edits(values, &path));
        }
    }
    if let Some(changes) = value.get("documentChanges").and_then(Value::as_array) {
        for change in changes {
            let Some(document) = change.get("textDocument") else { continue };
            let Some(uri) = document.get("uri").and_then(Value::as_str) else { continue };
            let Some(path) = uri_to_path(uri) else { continue };
            if let Some(version) = document.get("version").and_then(Value::as_u64) {
                edit.versions.push((path.clone(), version));
            }
            if let Some(edits) = change.get("edits") {
                edit.edits.extend(text_edits(edits, &path));
            }
        }
    }
    edit
}

fn code_actions(value: &Value) -> Vec<CodeAction> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .take(MAX_CODE_ACTIONS)
        .filter_map(|action| {
            Some(CodeAction {
                title: action.get("title")?.as_str()?.to_string(),
                kind: action.get("kind").and_then(Value::as_str).map(str::to_string),
                edit: action.get("edit").map(workspace_edit),
            })
        })
        .collect()
}

fn symbol_kind(kind: Option<u64>) -> SymbolKind {
    match kind {
        Some(1) => SymbolKind::File,
        Some(2) | Some(3) | Some(4) => SymbolKind::Module,
        Some(5) | Some(23) => SymbolKind::Struct,
        Some(6) => SymbolKind::Method,
        Some(7) | Some(8) => SymbolKind::Field,
        Some(9) | Some(12) => SymbolKind::Function,
        Some(10) => SymbolKind::Enum,
        Some(11) => SymbolKind::Trait,
        Some(13) => SymbolKind::Variable,
        Some(14) | Some(22) => SymbolKind::Constant,
        Some(26) => SymbolKind::TypeAlias,
        _ => SymbolKind::Other,
    }
}

fn completion_kind(kind: Option<u64>) -> SymbolKind {
    match kind {
        Some(2) => SymbolKind::Method,
        Some(3) | Some(4) => SymbolKind::Function,
        Some(5) | Some(10) => SymbolKind::Field,
        Some(6) => SymbolKind::Variable,
        Some(7) | Some(22) => SymbolKind::Struct,
        Some(8) => SymbolKind::Trait,
        Some(9) | Some(19) => SymbolKind::Module,
        Some(13) => SymbolKind::Enum,
        Some(15) => SymbolKind::Macro,
        Some(17) => SymbolKind::File,
        Some(20) | Some(21) => SymbolKind::Constant,
        Some(25) => SymbolKind::TypeAlias,
        _ => SymbolKind::Other,
    }
}

fn path_to_uri(path: &Path) -> String {
    let path = path.to_string_lossy();
    let mut encoded = String::with_capacity(path.len() + 7);
    encoded.push_str("file://");
    for byte in path.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'_' | b'.' | b'~' | b':') {
            encoded.push(char::from(byte));
        } else {
            use std::fmt::Write as _;
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    encoded
}

fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let encoded = uri.strip_prefix("file://")?;
    let mut decoded = Vec::with_capacity(encoded.len());
    let bytes = encoded.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let high = hex(bytes[index + 1])?;
            let low = hex(bytes[index + 2])?;
            decoded.push((high << 4) | low);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    Some(PathBuf::from(String::from_utf8(decoded).ok()?))
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Message;
    use std::path::PathBuf;
    use termesh_core::{
        DiagnosticOrigin, DiagnosticSeverity, LspEvent, LspRequest, LspRequestId, TextPosition,
    };

    fn root() -> PathBuf {
        PathBuf::from("/proj")
    }

    fn did_open() -> LspRequest {
        LspRequest::DidOpen {
            path: root().join("src/main.rs"),
            language_id: "rust".into(),
            version: 1,
            text: "fn main() {}".into(),
        }
    }

    fn response_to(message: &Message, result: serde_json::Value) -> Message {
        let Message::Request { id, .. } = message else {
            panic!("expected request, got {message:?}");
        };
        Message::Response { id: *id, result }
    }

    fn ready_translator() -> Translator {
        let mut translator = Translator::new();
        let initialize = translator.initialize(root());
        let _ =
            translator.incoming(response_to(&initialize, serde_json::json!({"capabilities": {}})));
        translator
    }

    #[test]
    fn initialize_declares_the_capabilities_we_actually_use() {
        let mut t = Translator::new();
        let Message::Request { method, params, .. } = t.initialize(root()) else {
            panic!("initialize is a request");
        };
        assert_eq!(method, "initialize");
        assert_eq!(params["capabilities"]["window"]["workDoneProgress"], true);
        assert!(params["capabilities"]["textDocument"]["synchronization"].is_object());
    }

    #[test]
    fn initialization_options_are_forwarded_when_a_recipe_supplies_them() {
        let mut t = Translator::new();
        let Message::Request { params, .. } = t
            .initialize_with(root(), Some(r#"{"settings":{"java":{"home":"/jdk"}}}"#))
            .expect("valid initializationOptions")
        else {
            panic!("initialize is a request");
        };
        assert_eq!(params["initializationOptions"]["settings"]["java"]["home"], "/jdk");
    }

    #[test]
    fn absent_initialization_options_are_omitted_not_sent_as_null() {
        let mut t = Translator::new();
        let Message::Request { params, .. } =
            t.initialize_with(root(), None).expect("no initializationOptions is valid")
        else {
            panic!("initialize is a request");
        };
        assert!(params.get("initializationOptions").is_none());
    }

    #[test]
    fn malformed_initialization_options_fail_the_handshake_with_a_named_reason() {
        let mut t = Translator::new();
        let failure = t.initialize_with(root(), Some("{not json")).expect_err("malformed");
        assert_eq!(failure.kind, termesh_core::LspFailureKind::Handshake);
        assert!(failure.message.contains("initializationOptions"), "{}", failure.message);
    }

    #[test]
    fn nothing_is_sent_before_the_server_answers_initialize() {
        let mut t = Translator::new();
        let _ = t.initialize(root());
        let queued = t.outgoing(did_open());
        assert!(queued.is_empty(), "requests queue until the handshake completes");
    }

    #[test]
    fn the_initialize_response_releases_the_queue_and_sends_initialized() {
        let mut t = Translator::new();
        let init = t.initialize(root());
        let _ = t.outgoing(did_open());
        let (events, messages) =
            t.incoming(response_to(&init, serde_json::json!({"capabilities": {}})));
        assert!(events.contains(&LspEvent::Ready));
        assert!(messages.iter().any(|m| matches!(m,
            Message::Notification { method, .. } if method == "initialized")));
        assert!(messages.iter().any(|m| matches!(m,
            Message::Notification { method, .. } if method == "textDocument/didOpen")));
    }

    #[test]
    fn a_client_request_we_do_not_implement_still_gets_an_answer() {
        let mut t = ready_translator();
        for method in [
            "client/registerCapability",
            "workspace/configuration",
            "window/workDoneProgress/create",
            "telemetry/event",
        ] {
            let (_, out) = t.incoming(Message::Request {
                id: 42,
                method: method.into(),
                params: serde_json::Value::Null,
            });
            assert!(!out.is_empty(), "{method} was left unanswered");
        }
    }

    #[test]
    fn an_unknown_method_is_answered_with_method_not_found() {
        let mut t = ready_translator();
        let (_, out) = t.incoming(Message::Request {
            id: 7,
            method: "server/inventedMethod".into(),
            params: serde_json::Value::Null,
        });
        assert!(matches!(out.as_slice(), [Message::Error { id: 7, code: -32601, .. }]));
    }

    #[test]
    fn published_diagnostics_become_typed_diagnostics() {
        let mut t = ready_translator();
        let (events, _) = t.incoming(Message::Notification {
            method: "textDocument/publishDiagnostics".into(),
            params: serde_json::json!({
                "uri": "file:///proj/src/main.rs",
                "version": 3,
                "diagnostics": [{
                    "range": {"start": {"line": 4, "character": 8},
                              "end":   {"line": 4, "character": 12}},
                    "severity": 1,
                    "code": "E0308",
                    "source": "rustc",
                    "message": "mismatched types"
                }]
            }),
        });
        let LspEvent::Diagnostics { path, version, items } = &events[0] else {
            panic!("expected diagnostics, got {events:?}");
        };
        assert_eq!(path, std::path::Path::new("/proj/src/main.rs"));
        assert_eq!(*version, Some(3));
        assert_eq!(items[0].severity, DiagnosticSeverity::Error);
        assert_eq!(items[0].origin, DiagnosticOrigin::LanguageServer);
        assert_eq!(items[0].range.start.line, 4);
    }

    #[test]
    fn every_severity_maps_and_an_unknown_one_does_not_panic() {
        let mut t = ready_translator();
        let expected = [
            DiagnosticSeverity::Error,
            DiagnosticSeverity::Warning,
            DiagnosticSeverity::Info,
            DiagnosticSeverity::Hint,
        ];
        for (severity, expected) in (1..=4).zip(expected) {
            let (events, _) = t.incoming(Message::Notification {
                method: "textDocument/publishDiagnostics".into(),
                params: serde_json::json!({
                    "uri":"file:///proj/src/main.rs",
                    "diagnostics":[{"range":{"start":{"line":0,"character":0},
                                               "end":{"line":0,"character":1}},
                                    "severity":severity,"message":"x"}]
                }),
            });
            let LspEvent::Diagnostics { items, .. } = &events[0] else { panic!() };
            assert_eq!(items[0].severity, expected);
        }

        let (events, _) = t.incoming(Message::Notification {
            method: "textDocument/publishDiagnostics".into(),
            params: serde_json::json!({
                "uri":"file:///proj/src/main.rs",
                "diagnostics":[{"range":{"start":{"line":0,"character":0},
                                           "end":{"line":0,"character":1}},
                                "severity":99,"message":"x"}]
            }),
        });
        assert!(matches!(events.as_slice(), [LspEvent::Diagnostics { .. }]));
    }

    #[test]
    fn progress_notifications_become_indexing_events() {
        let mut t = ready_translator();
        let (events, _) = t.incoming(Message::Notification {
            method: "$/progress".into(),
            params: serde_json::json!({
                "token":"rustAnalyzer/Indexing",
                "value":{"kind":"report","message":"indexing","percentage":42}
            }),
        });
        assert!(matches!(events.as_slice(), [LspEvent::Indexing { percent: Some(42), .. }]));
    }

    /// The status bar said "LSP indexing" for as long as the editor stayed open, on every
    /// project. Progress arrives as begin/report/end per token and every one of them was
    /// read as "still working", so the end — the only notification that means finished —
    /// kept the session in the same state it was trying to leave.
    #[test]
    fn the_last_progress_token_to_end_reports_the_server_ready() {
        let mut t = ready_translator();
        let progress = |token: &str, kind: &str| Message::Notification {
            method: "$/progress".into(),
            params: serde_json::json!({
                "token": token,
                "value": {"kind": kind, "message": "indexing", "percentage": 10}
            }),
        };

        // rust-analyzer runs several units of work at once.
        let (events, _) = t.incoming(progress("roots", "begin"));
        assert!(matches!(events.as_slice(), [LspEvent::Indexing { .. }]));
        let (events, _) = t.incoming(progress("indexing", "begin"));
        assert!(matches!(events.as_slice(), [LspEvent::Indexing { .. }]));

        // One finishing is not the server finishing.
        let (events, _) = t.incoming(progress("roots", "end"));
        assert!(events.is_empty(), "work is still outstanding: {events:?}");

        let (events, _) = t.incoming(progress("indexing", "end"));
        assert_eq!(events.as_slice(), [LspEvent::Ready], "the last one is");
    }

    /// Not every server announces a token. Before tracking them, every notification meant
    /// "working"; that stays true when there is nothing to track, rather than the status
    /// silently never appearing.
    #[test]
    fn progress_without_a_token_still_reports_work() {
        let mut t = ready_translator();
        let (events, _) = t.incoming(Message::Notification {
            method: "$/progress".into(),
            params: serde_json::json!({ "value": {"kind": "report", "message": "loading"} }),
        });
        assert!(matches!(events.as_slice(), [LspEvent::Indexing { .. }]));
    }

    #[test]
    fn a_jdt_language_status_notification_becomes_an_indexing_event() {
        // JDT LS reports import and index progress through its own notification as well
        // as `$/progress`. The long silent part of a cold start is this one, and without
        // it the status bar is blank for a minute and reads as a hang.
        let mut t = ready_translator();
        let (events, _) = t.incoming(Message::Notification {
            method: "language/status".into(),
            params: serde_json::json!({"type": "Starting", "message": "Importing projects..."}),
        });
        assert!(matches!(
            events.as_slice(),
            [LspEvent::Indexing { message, .. }] if message.contains("Importing")
        ));
    }

    #[test]
    fn a_ready_status_ends_indexing_rather_than_reporting_progress_forever() {
        let mut t = ready_translator();
        let (events, _) = t.incoming(Message::Notification {
            method: "language/status".into(),
            params: serde_json::json!({"type": "Started", "message": "Ready"}),
        });
        assert!(matches!(events.as_slice(), [LspEvent::Ready]));
    }

    #[test]
    fn an_unknown_jdt_notification_is_ignored_without_error() {
        let mut t = ready_translator();
        let (events, out) = t.incoming(Message::Notification {
            method: "language/eventNotification".into(),
            params: serde_json::json!({"eventType": "ClasspathUpdated"}),
        });
        assert!(events.is_empty());
        assert!(out.is_empty(), "a notification is never answered");
    }

    #[test]
    fn the_reload_encodes_each_path_as_a_jdt_notification() {
        let mut t = ready_translator();
        let out = t.outgoing(LspRequest::ReloadProject {
            paths: vec!["/p/pom.xml".into(), "/p/build.gradle".into()],
        });
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|message| matches!(
            message,
            Message::Notification { method, params }
                if method == "java/projectConfigurationUpdate"
                    && params.get("uri").and_then(serde_json::Value::as_str).is_some()
        )));
    }

    #[test]
    fn cancelling_a_request_emits_a_cancel_notification() {
        let mut t = ready_translator();
        let sent = t.outgoing(hover_request(1));
        let cancel = t.outgoing(LspRequest::Cancel { id: LspRequestId::new(1) });
        assert!(cancel.iter().any(|m| matches!(m,
            Message::Notification { method, .. } if method == "$/cancelRequest")));
        assert_eq!(sent.len(), 1);
    }

    #[test]
    fn a_reply_to_a_cancelled_request_produces_no_event() {
        let mut t = ready_translator();
        let sent = t.outgoing(hover_request(1));
        let _ = t.outgoing(LspRequest::Cancel { id: LspRequestId::new(1) });
        let (events, _) = t.incoming(response_to(&sent[0], serde_json::json!(null)));
        assert!(events.is_empty());
    }

    fn path() -> PathBuf {
        root().join("src/main.rs")
    }

    fn position() -> TextPosition {
        TextPosition { line: 4, character: 8 }
    }

    fn reply(t: &mut Translator, request: LspRequest, result: serde_json::Value) -> Vec<LspEvent> {
        let messages = t.outgoing(request);
        assert_eq!(messages.len(), 1, "ready request should be sent immediately");
        t.incoming(response_to(&messages[0], result)).0
    }

    fn wire_range() -> serde_json::Value {
        serde_json::json!({
            "start":{"line":4,"character":8},
            "end":{"line":4,"character":12}
        })
    }

    #[test]
    fn definition_maps_single_and_array_locations_with_the_correlation_id() {
        let mut t = ready_translator();
        let request = LspRequest::Definition {
            id: LspRequestId::new(10),
            path: path(),
            position: position(),
        };
        let sent = t.outgoing(request);
        let Message::Request { params, .. } = &sent[0] else { panic!() };
        assert_eq!(params["position"], serde_json::json!({"line":4,"character":8}));
        let (events, _) = t.incoming(response_to(
            &sent[0],
            serde_json::json!({"uri":"file:///proj/src/lib.rs","range":wire_range()}),
        ));
        assert!(matches!(events.as_slice(),
            [LspEvent::Definition { id, locations }] if *id == LspRequestId::new(10) && locations.len() == 1));

        let events = reply(
            &mut t,
            LspRequest::Definition {
                id: LspRequestId::new(11),
                path: path(),
                position: position(),
            },
            serde_json::json!([
                {"uri":"file:///proj/src/a.rs","range":wire_range()},
                {"uri":"file:///proj/src/b.rs","range":wire_range()}
            ]),
        );
        assert!(matches!(events.as_slice(),
            [LspEvent::Definition { id, locations }] if *id == LspRequestId::new(11) && locations.len() == 2));
    }

    #[test]
    fn hover_maps_markup_content_and_the_legacy_string_form() {
        let mut t = ready_translator();
        let markup = reply(
            &mut t,
            hover_request(20),
            serde_json::json!({
                "contents":{"kind":"markdown","value":"**main**"},
                "range":wire_range()
            }),
        );
        assert!(matches!(markup.as_slice(),
            [LspEvent::Hover { id, hover: Some(hover) }]
                if *id == LspRequestId::new(20) && hover.text == "**main**" && hover.range.is_some()));

        let legacy = reply(&mut t, hover_request(21), serde_json::json!({"contents":"fn main()"}));
        assert!(matches!(legacy.as_slice(),
            [LspEvent::Hover { id, hover: Some(hover) }]
                if *id == LspRequestId::new(21) && hover.text == "fn main()"));
    }

    #[test]
    fn completion_maps_both_a_list_and_a_completion_list() {
        let mut t = ready_translator();
        for (id, result, expected_kind) in [
            (
                30,
                serde_json::json!([{"label":"main","kind":3,"insertText":"main"}]),
                termesh_core::SymbolKind::Function,
            ),
            (
                31,
                serde_json::json!({"isIncomplete":false,"items":[{
                    "label":"println!","detail":"macro","kind":14,"insertText":"println!"
                }]}),
                termesh_core::SymbolKind::Other,
            ),
        ] {
            let events = reply(
                &mut t,
                LspRequest::Completion {
                    id: LspRequestId::new(id),
                    path: path(),
                    position: position(),
                },
                result,
            );
            assert!(matches!(events.as_slice(),
                [LspEvent::Completion { id: event_id, items }]
                    if *event_id == LspRequestId::new(id) && items.len() == 1
                        && items[0].kind == expected_kind));
        }
    }

    #[test]
    fn references_map_locations_with_the_correlation_id() {
        let mut t = ready_translator();
        let events = reply(
            &mut t,
            LspRequest::References {
                id: LspRequestId::new(40),
                path: path(),
                position: position(),
            },
            serde_json::json!([{"uri":"file:///proj/src/main.rs","range":wire_range()}]),
        );
        assert!(matches!(events.as_slice(),
            [LspEvent::References { id, locations }] if *id == LspRequestId::new(40) && locations.len() == 1));
    }

    #[test]
    fn document_symbols_map_hierarchical_and_flat_shapes() {
        let mut t = ready_translator();
        let hierarchical = reply(
            &mut t,
            LspRequest::DocumentSymbols { id: LspRequestId::new(50), path: path() },
            serde_json::json!([{
                "name":"outer","kind":12,"range":wire_range(),"selectionRange":wire_range(),
                "children":[{"name":"inner","kind":6,"range":wire_range(),
                             "selectionRange":wire_range()}]
            }]),
        );
        assert!(matches!(hierarchical.as_slice(),
            [LspEvent::DocumentSymbols { id, symbols }]
                if *id == LspRequestId::new(50) && symbols[0].children.len() == 1));

        let flat = reply(
            &mut t,
            LspRequest::DocumentSymbols { id: LspRequestId::new(51), path: path() },
            serde_json::json!([{
                "name":"flat","kind":12,"containerName":"module",
                "location":{"uri":"file:///proj/src/main.rs","range":wire_range()}
            }]),
        );
        assert!(matches!(flat.as_slice(),
            [LspEvent::DocumentSymbols { id, symbols }]
                if *id == LspRequestId::new(51) && symbols[0].detail.as_deref() == Some("module")));
    }

    #[test]
    fn workspace_symbols_map_locations_with_the_correlation_id() {
        let mut t = ready_translator();
        let events = reply(
            &mut t,
            LspRequest::WorkspaceSymbols { id: LspRequestId::new(60), query: "main".into() },
            serde_json::json!([{
                "name":"main","kind":12,"containerName":"crate",
                "location":{"uri":"file:///proj/src/main.rs","range":wire_range()}
            }]),
        );
        assert!(matches!(events.as_slice(),
            [LspEvent::WorkspaceSymbols { id, symbols }]
                if *id == LspRequestId::new(60) && symbols[0].name == "main"));
    }

    #[test]
    fn rename_maps_a_versioned_workspace_edit() {
        let mut t = ready_translator();
        let events = reply(
            &mut t,
            LspRequest::Rename {
                id: LspRequestId::new(70),
                path: path(),
                position: position(),
                new_name: "run".into(),
            },
            serde_json::json!({"documentChanges":[{
                "textDocument":{"uri":"file:///proj/src/main.rs","version":3},
                "edits":[{"range":wire_range(),"newText":"run"}]
            }]}),
        );
        assert!(matches!(events.as_slice(),
            [LspEvent::Rename { id, edit }]
                if *id == LspRequestId::new(70) && edit.edits.len() == 1
                    && edit.versions == vec![(path(), 3)]));
    }

    #[test]
    fn code_actions_map_titles_kinds_and_workspace_edits() {
        let mut t = ready_translator();
        let events = reply(
            &mut t,
            LspRequest::CodeActions {
                id: LspRequestId::new(80),
                path: path(),
                range: termesh_core::TextRange { start: position(), end: position() },
            },
            serde_json::json!([{
                "title":"Import Foo","kind":"quickfix",
                "edit":{"changes":{"file:///proj/src/main.rs":[{
                    "range":wire_range(),"newText":"use foo::Foo;\n"
                }]}}
            }]),
        );
        assert!(matches!(events.as_slice(),
            [LspEvent::CodeActions { id, actions }]
                if *id == LspRequestId::new(80) && actions[0].kind.as_deref() == Some("quickfix")
                    && actions[0].edit.as_ref().is_some_and(|edit| edit.edits.len() == 1)));
    }

    #[test]
    fn formatting_maps_edits_with_the_request_path() {
        let mut t = ready_translator();
        let events = reply(
            &mut t,
            LspRequest::Formatting { id: LspRequestId::new(90), path: path() },
            serde_json::json!([{"range":wire_range(),"newText":"fn main() {}\n"}]),
        );
        assert!(matches!(events.as_slice(),
            [LspEvent::Formatting { id, edits }]
                if *id == LspRequestId::new(90) && edits[0].path == path()));
    }

    #[allow(dead_code)]
    fn hover_request(id: u64) -> LspRequest {
        LspRequest::Hover {
            id: LspRequestId::new(id),
            path: root().join("src/main.rs"),
            position: TextPosition { line: 0, character: 3 },
        }
    }
}
