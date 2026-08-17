//! Translating between our vocabulary and the ACP wire (ADR-0007 §1).
//!
//! Deliberately pure: [`Translator`] takes messages and returns messages, with no
//! process, threads, or I/O anywhere in it. The transport in [`crate::acp`] is a thin
//! shell around this, which is why the protocol can be tested exhaustively without an
//! agent installed — the same "pure logic, thin I/O shell" split `filesystem` uses for
//! the tree and the worker.
//!
//! This is also the isolation boundary ADR-0003 asks for: every ACP field name in the
//! codebase appears in this file and nowhere else, so a protocol change is a diff here
//! rather than an archaeology exercise.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};
use termesh_core::{
    AgentCapabilities, AgentEvent, AgentRequest, AgentTerminalOperation, AgentTerminalRequestId,
    AgentTerminalResponse, PermissionDecision, PermissionRequestId, PromptCapabilities, ProposalId,
    ReadRequestId, SessionId, SessionMode, StopReason, TerminalExit, TerminalId, TerminalSpec,
};

use crate::jsonrpc::{Message, RequestIds};
use crate::service::ClientCapabilities;

/// The protocol version we speak.
const PROTOCOL_VERSION: u64 = 1;
const DEFAULT_OUTPUT_LIMIT: usize = 1_048_576;
const MAX_OUTPUT_LIMIT: usize = 8_388_608;

/// What we were doing when we sent a request, so its response means something.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Pending {
    Initialize,
    NewSession,
    Prompt(SessionId),
    SetMode { session: SessionId, mode: String },
}

/// Stateful but I/O-free translation in both directions.
#[derive(Debug, Default)]
pub struct Translator {
    ids: RequestIds,
    pending: HashMap<u64, Pending>,
    /// Ours ↔ theirs. ACP session ids are opaque strings; ours are typed integers
    /// (ARCHITECTURE.md §7.3 — never key identity on someone else's string).
    sessions: Vec<(SessionId, String)>,
    /// Permission requests we must answer, by our id.
    permissions: HashMap<PermissionRequestId, PendingPermission>,
    /// Reads the agent is waiting on, by *our* id — never by path. An agent may read the
    /// same file twice in a turn, and a path-keyed map would drop one of the two.
    reads: HashMap<ReadRequestId, u64>,
    terminal_rpcs: HashMap<AgentTerminalRequestId, PendingTerminalRpc>,
    terminals: Vec<TerminalBinding>,
    one_shot_terminal_grants: Vec<(SessionId, TerminalSpec)>,
    next_session: u64,
    next_id: u64,
    /// Whether `initialize` has completed. Requests queued before then are held.
    ready: bool,
    queued: Vec<AgentRequest>,
}

/// One choice offered on a permission request.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PermissionOption {
    id: String,
    kind: String,
}

#[derive(Debug, Clone)]
struct PendingPermission {
    wire_request: u64,
    session: SessionId,
    options: Vec<PermissionOption>,
    terminal_spec: Option<TerminalSpec>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TerminalRpcKind {
    Create,
    Output(TerminalId),
    Wait(TerminalId),
    Kill(TerminalId),
    Release(TerminalId),
}

#[derive(Debug, Clone, Copy)]
struct PendingTerminalRpc {
    wire_request: u64,
    session: SessionId,
    kind: TerminalRpcKind,
}

#[derive(Debug, Clone)]
struct TerminalBinding {
    local: TerminalId,
    wire: String,
    session: SessionId,
    released: bool,
}

impl Translator {
    pub fn new() -> Self {
        Self::default()
    }

    /// The opening handshake. Sent once, before anything else.
    pub fn initialize(&mut self, capabilities: ClientCapabilities) -> Message {
        let id = self.ids.allocate();
        self.pending.insert(id, Pending::Initialize);
        Message::Request {
            id,
            method: "initialize".into(),
            params: json!({
                "protocolVersion": PROTOCOL_VERSION,
                "clientCapabilities": {
                    "fs": {
                        "readTextFile": capabilities.read_text_file,
                        "writeTextFile": capabilities.write_text_file,
                    },
                    "terminal": capabilities.terminal,
                }
            }),
        }
    }

    fn wire_session(&self, session: SessionId) -> Option<&str> {
        self.sessions.iter().find(|(ours, _)| *ours == session).map(|(_, wire)| wire.as_str())
    }

    fn our_session(&self, wire: &str) -> Option<SessionId> {
        self.sessions.iter().find(|(_, theirs)| theirs == wire).map(|(ours, _)| *ours)
    }

    fn terminal_binding(&self, wire: &str) -> Option<&TerminalBinding> {
        self.terminals.iter().find(|binding| binding.wire == wire)
    }

    fn live_terminal(&self, wire: &str, session: SessionId) -> Option<TerminalId> {
        self.terminal_binding(wire)
            .filter(|binding| binding.session == session && !binding.released)
            .map(|binding| binding.local)
    }

    fn allocate_terminal_request(
        &mut self,
        wire_request: u64,
        session: SessionId,
        kind: TerminalRpcKind,
        operation: AgentTerminalOperation,
    ) -> AgentEvent {
        self.next_id += 1;
        let request = AgentTerminalRequestId::new(self.next_id);
        self.terminal_rpcs.insert(request, PendingTerminalRpc { wire_request, session, kind });
        AgentEvent::TerminalRequest { session, request, operation }
    }

    /// Turn one of our requests into wire messages.
    ///
    /// Anything sent before `initialize` completes is queued rather than dropped: a user
    /// who starts a session the instant the app opens should not lose it to a race.
    pub fn outgoing(&mut self, request: AgentRequest) -> Vec<Message> {
        if !self.ready && !matches!(request, AgentRequest::Shutdown) {
            self.queued.push(request);
            return Vec::new();
        }
        self.encode(request).into_iter().collect()
    }

    fn encode(&mut self, request: AgentRequest) -> Option<Message> {
        match request {
            AgentRequest::NewSession { cwd } => {
                let id = self.ids.allocate();
                self.pending.insert(id, Pending::NewSession);
                Some(Message::Request {
                    id,
                    method: "session/new".into(),
                    // No MCP servers of our own: the agent brings its own tooling, and we
                    // are the filesystem it talks to (ADR-0007 §3).
                    params: json!({ "cwd": cwd, "mcpServers": [] }),
                })
            }
            AgentRequest::SetMode { session, mode } => {
                let wire = self.wire_session(session)?.to_string();
                let id = self.ids.allocate();
                // The success reply is the agent's own statement that it changed the mode,
                // so it is what moves this client — not a guess we render before hearing
                // back. Waiting for `current_mode_update` instead would strand the pane:
                // codex-acp answers `{}` and never notifies (ADR-0015 §5).
                self.pending.insert(id, Pending::SetMode { session, mode: mode.clone() });
                Some(Message::Request {
                    id,
                    method: "session/set_mode".into(),
                    params: json!({ "sessionId": wire, "modeId": mode }),
                })
            }
            AgentRequest::Prompt { session, text, context } => {
                let wire = self.wire_session(session)?.to_string();
                let id = self.ids.allocate();
                self.pending.insert(id, Pending::Prompt(session));
                // Context first, then the user's words — the snapshot is framing, not the
                // question (ADR-0007 §4).
                let blocks = if context.is_empty() {
                    vec![json!({ "type": "text", "text": text })]
                } else {
                    vec![
                        json!({ "type": "text", "text": context }),
                        json!({ "type": "text", "text": text }),
                    ]
                };
                Some(Message::Request {
                    id,
                    method: "session/prompt".into(),
                    params: json!({ "sessionId": wire, "prompt": blocks }),
                })
            }
            AgentRequest::FileContents { request, path, contents, .. } => {
                let request_id = self.reads.remove(&request)?;
                Some(match contents {
                    Some(content) => {
                        Message::Response { id: request_id, result: json!({ "content": content }) }
                    }
                    // Refusing a read is an error response, not an empty file — an agent
                    // told a file is empty will happily "fix" it by rewriting it whole.
                    None => Message::Error {
                        id: request_id,
                        code: -32000,
                        message: format!("cannot read {}", path.display()),
                    },
                })
            }
            AgentRequest::Permission { request, decision } => {
                let pending = self.permissions.remove(&request)?;
                let option = choose_option(&pending.options, decision);
                if option.is_some() && decision.allows() {
                    if let Some(spec) = pending.terminal_spec {
                        self.one_shot_terminal_grants.push((pending.session, spec));
                    }
                }
                Some(match option {
                    Some(id) => Message::Response {
                        id: pending.wire_request,
                        result: json!({ "outcome": { "outcome": "selected", "optionId": id } }),
                    },
                    // No matching option offered: cancelling is the protocol's way of
                    // saying "not this", and is safer than picking one we do not mean.
                    None => Message::Response {
                        id: pending.wire_request,
                        result: json!({ "outcome": { "outcome": "cancelled" } }),
                    },
                })
            }
            AgentRequest::PermissionCancelled { request } => {
                let pending = self.permissions.remove(&request)?;
                Some(Message::Response {
                    id: pending.wire_request,
                    result: json!({ "outcome": { "outcome": "cancelled" } }),
                })
            }
            AgentRequest::TerminalResponse { request, response } => {
                self.encode_terminal_response(request, response)
            }
            AgentRequest::Cancel { session } => {
                self.expire_terminal_grants(session);
                let wire = self.wire_session(session)?.to_string();
                Some(Message::Notification {
                    method: "session/cancel".into(),
                    params: json!({ "sessionId": wire }),
                })
            }
            AgentRequest::Shutdown => None,
        }
    }

    /// Drop any "allow once" terminal grant the agent did not spend.
    ///
    /// A grant is scoped to the turn it was given in (ADR-0008 §5). Left to accumulate,
    /// a grant approved in one turn would silently preauthorize an identical
    /// `terminal/create` many turns later — a launch the user was never asked about.
    fn expire_terminal_grants(&mut self, session: SessionId) {
        self.one_shot_terminal_grants.retain(|(owner, _)| *owner != session);
    }

    fn encode_terminal_response(
        &mut self,
        request: AgentTerminalRequestId,
        response: AgentTerminalResponse,
    ) -> Option<Message> {
        let pending = self.terminal_rpcs.remove(&request)?;
        if let AgentTerminalResponse::Error(message) = response {
            return Some(Message::Error { id: pending.wire_request, code: -32000, message });
        }

        let result = match (pending.kind, response) {
            (TerminalRpcKind::Create, AgentTerminalResponse::Created { terminal }) => {
                if self.terminals.iter().any(|binding| binding.local == terminal) {
                    return Some(Message::Error {
                        id: pending.wire_request,
                        code: -32000,
                        message: format!("terminal {terminal} already has a wire id"),
                    });
                }
                let wire = format!("termesh-{}", terminal.0);
                self.terminals.push(TerminalBinding {
                    local: terminal,
                    wire: wire.clone(),
                    session: pending.session,
                    released: false,
                });
                json!({ "terminalId": wire })
            }
            (
                TerminalRpcKind::Output(_),
                AgentTerminalResponse::Output { output, truncated, exit },
            ) => json!({
                "output": output,
                "truncated": truncated,
                "exitStatus": exit.map(exit_status),
            }),
            (TerminalRpcKind::Wait(_), AgentTerminalResponse::Exited(exit)) => exit_status(exit),
            (TerminalRpcKind::Kill(_), AgentTerminalResponse::Acknowledged) => json!({}),
            (TerminalRpcKind::Release(terminal), AgentTerminalResponse::Acknowledged) => {
                if let Some(binding) = self
                    .terminals
                    .iter_mut()
                    .find(|binding| binding.local == terminal && binding.session == pending.session)
                {
                    binding.released = true;
                }
                json!({})
            }
            (_, _) => {
                return Some(Message::Error {
                    id: pending.wire_request,
                    code: -32000,
                    message: "terminal response did not match its request".into(),
                });
            }
        };
        Some(Message::Response { id: pending.wire_request, result })
    }

    /// Absorb one wire message: what the model should hear, and what we must send back.
    pub fn incoming(&mut self, message: Message) -> (Vec<AgentEvent>, Vec<Message>) {
        match message {
            Message::Response { id, result } => self.on_response(id, result),
            Message::Error { id, message, .. } => self.on_error(id, message),
            Message::Notification { method, params } => {
                (self.on_notification(&method, params), vec![])
            }
            Message::Request { id, method, params } => self.on_request(id, &method, params),
        }
    }

    fn on_response(&mut self, id: u64, result: Value) -> (Vec<AgentEvent>, Vec<Message>) {
        match self.pending.remove(&id) {
            Some(Pending::Initialize) => {
                self.ready = true;
                let capabilities = parse_agent_capabilities(&result);
                // Anything the user asked for during the handshake goes out now, in order.
                // The drain must run whether or not the result parsed cleanly — a queued
                // request must never wait forever on a malformed handshake.
                let queued = std::mem::take(&mut self.queued);
                let messages = queued.into_iter().filter_map(|r| self.encode(r)).collect();
                (vec![AgentEvent::Ready { capabilities }], messages)
            }
            Some(Pending::NewSession) => {
                let Some(wire) = result.get("sessionId").and_then(Value::as_str) else {
                    return (
                        vec![AgentEvent::Failed {
                            session: SessionId::new(0),
                            message: "session/new returned no sessionId".into(),
                        }],
                        vec![],
                    );
                };
                self.next_session += 1;
                let session = SessionId::new(self.next_session);
                self.sessions.push((session, wire.to_string()));

                // Modes are optional and most agents omit them, so their absence is not
                // a failure — it means this session has one mode and no choice to offer
                // (ADR-0015 §4).
                let mut events = vec![AgentEvent::SessionStarted { session }];
                if let Some(modes) = parse_session_modes(session, result.get("modes")) {
                    events.push(modes);
                }
                (events, vec![])
            }
            Some(Pending::Prompt(session)) => {
                let reason = match result.get("stopReason").and_then(Value::as_str) {
                    Some("cancelled") => StopReason::Cancelled,
                    Some("refusal") => StopReason::Refusal,
                    Some("max_tokens") => StopReason::MaxTokens,
                    _ => StopReason::EndTurn,
                };
                self.expire_terminal_grants(session);
                (vec![AgentEvent::TurnEnded { session, reason }], vec![])
            }
            Some(Pending::SetMode { session, mode }) => {
                (vec![AgentEvent::ModeChanged { session, mode }], vec![])
            }
            None => (vec![], vec![]),
        }
    }

    fn on_error(&mut self, id: u64, message: String) -> (Vec<AgentEvent>, Vec<Message>) {
        let session = match self.pending.remove(&id) {
            Some(Pending::Prompt(session)) => session,
            // A refused mode change reports the refusal and leaves the mode alone — the
            // agent said no, so the pane must keep showing what the agent is still in.
            Some(Pending::SetMode { session, .. }) => session,
            _ => SessionId::new(0),
        };
        // A prompt that errors out ends its turn as surely as one that completes, so any
        // unspent grant expires here too — otherwise the turn scope in
        // `expire_terminal_grants` has a door left open.
        self.expire_terminal_grants(session);
        (vec![AgentEvent::Failed { session, message }], vec![])
    }

    fn on_notification(&mut self, method: &str, params: Value) -> Vec<AgentEvent> {
        if method != "session/update" {
            return Vec::new(); // an update we do not model yet; ignoring is correct
        }
        let Some(session) =
            params.get("sessionId").and_then(Value::as_str).and_then(|w| self.our_session(w))
        else {
            return Vec::new();
        };
        let Some(update) = params.get("update") else { return Vec::new() };
        let kind = update.get("sessionUpdate").and_then(Value::as_str).unwrap_or_default();

        match kind {
            "agent_message_chunk" => text_of(update)
                .map(|text| vec![AgentEvent::MessageChunk { session, text }])
                .unwrap_or_default(),
            "agent_thought_chunk" => text_of(update)
                .map(|text| vec![AgentEvent::ThoughtChunk { session, text }])
                .unwrap_or_default(),
            // Edits ride in on tool calls, as whole-file diffs (ADR-0007, finding 2).
            "tool_call" | "tool_call_update" => self.events_from_tool_call(session, update),
            // The agent's own account of the session's mode, which is the one that counts
            // — including when it differs from what we asked for (ADR-0015 §5).
            "current_mode_update" => update
                .get("modeId")
                .and_then(Value::as_str)
                .map(|mode| vec![AgentEvent::ModeChanged { session, mode: mode.to_string() }])
                .unwrap_or_default(),
            _ => Vec::new(),
        }
    }

    fn events_from_tool_call(&mut self, session: SessionId, update: &Value) -> Vec<AgentEvent> {
        let Some(contents) = update.get("content").and_then(Value::as_array) else {
            return Vec::new();
        };
        let mut events = Vec::new();
        for content in contents {
            match content.get("type").and_then(Value::as_str) {
                Some("diff") => {
                    let (Some(path), Some(new_text)) = (
                        content.get("path").and_then(Value::as_str),
                        content.get("newText").and_then(Value::as_str),
                    ) else {
                        continue;
                    };
                    self.next_id += 1;
                    events.push(AgentEvent::ProposedEdit {
                        session,
                        proposal: ProposalId::new(self.next_id),
                        path: PathBuf::from(path),
                        old_text: content
                            .get("oldText")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                        new_text: new_text.to_string(),
                    });
                }
                Some("terminal") => {
                    let Some(wire) = content.get("terminalId").and_then(Value::as_str) else {
                        continue;
                    };
                    if let Some(binding) =
                        self.terminal_binding(wire).filter(|binding| binding.session == session)
                    {
                        events.push(AgentEvent::TerminalAttached {
                            session,
                            terminal: binding.local,
                        });
                    }
                }
                _ => {}
            }
        }
        events
    }

    /// A call *from* the agent. Both of these need an answer, and until they get one the
    /// agent is blocked — so nothing here may quietly drop the id.
    fn on_request(
        &mut self,
        id: u64,
        method: &str,
        params: Value,
    ) -> (Vec<AgentEvent>, Vec<Message>) {
        let session = params
            .get("sessionId")
            .and_then(Value::as_str)
            .and_then(|w| self.our_session(w))
            .unwrap_or(SessionId::new(0));

        match method {
            "fs/read_text_file" => {
                let Some(path) = params.get("path").and_then(Value::as_str) else {
                    return (
                        vec![],
                        vec![Message::Error {
                            id,
                            code: -32602,
                            message: "fs/read_text_file needs a path".into(),
                        }],
                    );
                };
                self.next_id += 1;
                let request = ReadRequestId::new(self.next_id);
                self.reads.insert(request, id);
                (
                    vec![AgentEvent::ReadFileRequested {
                        session,
                        request,
                        path: PathBuf::from(path),
                    }],
                    vec![],
                )
            }
            // A write the agent wants to make. We accept responsibility for the content
            // and answer OK — which is what advertising the capability *means* — but it
            // lands as a proposal in the buffer, never on disk (ADR-0007 §3). Without
            // this the agent gets "not supported" for a capability we advertised, and
            // writes the file itself instead: exactly the unreviewed side effect the
            // capability exists to prevent.
            "fs/write_text_file" => {
                let (Some(path), Some(content)) = (
                    params.get("path").and_then(Value::as_str),
                    params.get("content").and_then(Value::as_str),
                ) else {
                    return (
                        vec![],
                        vec![Message::Error {
                            id,
                            code: -32602,
                            message: "fs/write_text_file needs a path and content".into(),
                        }],
                    );
                };

                self.next_id += 1;
                (
                    vec![AgentEvent::ProposedEdit {
                        session,
                        proposal: ProposalId::new(self.next_id),
                        path: PathBuf::from(path),
                        // The agent did not tell us what it was editing from; the client
                        // knows, because the client owns the buffer.
                        old_text: None,
                        new_text: content.to_string(),
                    }],
                    vec![Message::Response { id, result: Value::Null }],
                )
            }
            "session/request_permission" => {
                let options: Vec<PermissionOption> = params
                    .get("options")
                    .and_then(Value::as_array)
                    .map(|opts| {
                        opts.iter()
                            .filter_map(|o| {
                                Some(PermissionOption {
                                    id: o.get("optionId").and_then(Value::as_str)?.to_string(),
                                    kind: o
                                        .get("kind")
                                        .and_then(Value::as_str)
                                        .unwrap_or_default()
                                        .to_string(),
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default();

                let tool = params.get("toolCall");
                let summary = tool
                    .and_then(|t| t.get("title"))
                    .and_then(Value::as_str)
                    .unwrap_or("run a tool")
                    .to_string();
                let command = argv_of(tool);
                let terminal_spec = terminal_spec_of_permission(tool);

                self.next_id += 1;
                let request = PermissionRequestId::new(self.next_id);
                self.permissions.insert(
                    request,
                    PendingPermission {
                        wire_request: id,
                        session,
                        options,
                        terminal_spec: terminal_spec.clone(),
                    },
                );

                (
                    vec![AgentEvent::PermissionRequested {
                        session,
                        request,
                        summary,
                        command,
                        terminal_spec,
                    }],
                    vec![],
                )
            }
            "terminal/create" => {
                if session == SessionId::new(0) {
                    return invalid_params(id, "terminal/create needs a known sessionId");
                }
                let spec = match terminal_spec_of_create(&params) {
                    Ok(spec) => spec,
                    Err(message) => return invalid_params(id, message),
                };
                let output_byte_limit = match output_limit(&params) {
                    Ok(limit) => limit,
                    Err(message) => return invalid_params(id, message),
                };
                let preauthorized = self
                    .one_shot_terminal_grants
                    .iter()
                    .position(|(owner, granted)| *owner == session && *granted == spec)
                    .map(|index| {
                        self.one_shot_terminal_grants.remove(index);
                        true
                    })
                    .unwrap_or(false);
                let event = self.allocate_terminal_request(
                    id,
                    session,
                    TerminalRpcKind::Create,
                    AgentTerminalOperation::Create { spec, output_byte_limit, preauthorized },
                );
                (vec![event], vec![])
            }
            "terminal/output" | "terminal/wait_for_exit" | "terminal/kill" | "terminal/release" => {
                if session == SessionId::new(0) {
                    return invalid_params(id, format!("{method} needs a known sessionId"));
                }
                let Some(wire) = params.get("terminalId").and_then(Value::as_str) else {
                    return invalid_params(id, format!("{method} needs a terminalId"));
                };
                let Some(terminal) = self.live_terminal(wire, session) else {
                    return invalid_params(id, format!("unknown or released terminalId: {wire}"));
                };
                let (kind, operation) = match method {
                    "terminal/output" => (
                        TerminalRpcKind::Output(terminal),
                        AgentTerminalOperation::Output { terminal },
                    ),
                    "terminal/wait_for_exit" => (
                        TerminalRpcKind::Wait(terminal),
                        AgentTerminalOperation::WaitForExit { terminal },
                    ),
                    "terminal/kill" => {
                        (TerminalRpcKind::Kill(terminal), AgentTerminalOperation::Kill { terminal })
                    }
                    "terminal/release" => (
                        TerminalRpcKind::Release(terminal),
                        AgentTerminalOperation::Release { terminal },
                    ),
                    _ => unreachable!("matched terminal methods above"),
                };
                let event = self.allocate_terminal_request(id, session, kind, operation);
                (vec![event], vec![])
            }
            // An unknown call still gets an answer; leaving the agent blocked forever is
            // the one thing we must not do.
            _ => (
                vec![],
                vec![Message::Error {
                    id,
                    code: -32601,
                    message: format!("{method} is not supported"),
                }],
            ),
        }
    }
}

/// The `modes` object from a `session/new` result, if the agent sent one.
///
/// A mode with no id is unusable — it could never be named in `session/set_mode` — so it
/// is dropped rather than offered. An empty or malformed object yields nothing at all,
/// which reads the same as an agent that has no modes to offer (ADR-0015 §4).
fn parse_session_modes(session: SessionId, modes: Option<&Value>) -> Option<AgentEvent> {
    let modes = modes?;
    let current = modes.get("currentModeId").and_then(Value::as_str)?;
    let available: Vec<SessionMode> = modes
        .get("availableModes")
        .and_then(Value::as_array)?
        .iter()
        .filter_map(|mode| {
            let id = mode.get("id").and_then(Value::as_str)?.to_string();
            Some(SessionMode {
                // Falling back to the id keeps an unnamed mode selectable rather than
                // rendering a blank row in the picker.
                name: mode.get("name").and_then(Value::as_str).unwrap_or(&id).to_string(),
                description: mode.get("description").and_then(Value::as_str).map(str::to_string),
                id,
            })
        })
        .collect();
    if available.is_empty() {
        return None;
    }
    Some(AgentEvent::ModesAvailable { session, current: current.to_string(), available })
}

fn terminal_spec_of_create(params: &Value) -> Result<TerminalSpec, String> {
    let program = params
        .get("command")
        .and_then(Value::as_str)
        .filter(|program| !program.is_empty())
        .ok_or_else(|| "terminal/create needs a non-empty command".to_string())?;
    let args = string_array(params.get("args"), false, "terminal/create args")?;
    let cwd = params
        .get("cwd")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .filter(|path| valid_absolute_path(path))
        .ok_or_else(|| {
            "terminal/create cwd must be an absolute path without traversal".to_string()
        })?;
    let env = env_array(params.get("env"), false, "terminal/create env")?;
    Ok(TerminalSpec { program: program.into(), args, cwd, env })
}

fn terminal_spec_of_permission(tool: Option<&Value>) -> Option<TerminalSpec> {
    let raw = tool?.get("rawInput")?;
    let command = string_array(raw.get("command"), true, "permission command").ok()?;
    let (program, args) = command.split_first()?;
    let cwd = raw.get("cwd")?.as_str().map(PathBuf::from)?;
    if !valid_absolute_path(&cwd) {
        return None;
    }
    let env = env_array(raw.get("env"), true, "permission env").ok()?;
    Some(TerminalSpec { program: program.clone(), args: args.to_vec(), cwd, env })
}

fn string_array(value: Option<&Value>, required: bool, label: &str) -> Result<Vec<String>, String> {
    let Some(value) = value else {
        return if required { Err(format!("{label} must be an array")) } else { Ok(Vec::new()) };
    };
    let array = value.as_array().ok_or_else(|| format!("{label} must be an array"))?;
    array
        .iter()
        .map(|item| {
            item.as_str()
                .map(str::to_owned)
                .ok_or_else(|| format!("{label} must contain only strings"))
        })
        .collect()
}

fn env_array(
    value: Option<&Value>,
    required: bool,
    label: &str,
) -> Result<Vec<(String, String)>, String> {
    let Some(value) = value else {
        return if required { Err(format!("{label} must be an array")) } else { Ok(Vec::new()) };
    };
    let array = value.as_array().ok_or_else(|| format!("{label} must be an array"))?;
    array
        .iter()
        .map(|item| {
            let name = item
                .get("name")
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty() && !name.contains(['=', '\0']))
                .ok_or_else(|| format!("{label} entries need a valid name"))?;
            let value = item
                .get("value")
                .and_then(Value::as_str)
                .filter(|value| !value.contains('\0'))
                .ok_or_else(|| format!("{label} entries need a string value"))?;
            Ok((name.to_owned(), value.to_owned()))
        })
        .collect()
}

fn valid_absolute_path(path: &Path) -> bool {
    path.is_absolute()
        && !path.components().any(|component| matches!(component, std::path::Component::ParentDir))
}

fn output_limit(params: &Value) -> Result<usize, String> {
    let Some(value) = params.get("outputByteLimit") else {
        return Ok(DEFAULT_OUTPUT_LIMIT);
    };
    let limit = value
        .as_u64()
        .ok_or_else(|| "outputByteLimit must be a non-negative integer".to_string())?;
    Ok(limit.min(MAX_OUTPUT_LIMIT as u64) as usize)
}

fn exit_status(exit: TerminalExit) -> Value {
    json!({ "exitCode": exit.code, "signal": exit.signal })
}

fn invalid_params(id: u64, message: impl Into<String>) -> (Vec<AgentEvent>, Vec<Message>) {
    (vec![], vec![Message::Error { id, code: -32602, message: message.into() }])
}

/// Pull the text out of a content block.
fn text_of(update: &Value) -> Option<String> {
    update.get("content")?.get("text")?.as_str().map(str::to_string)
}

/// Read `agentCapabilities` from the `initialize` result. Absent means absent — a field
/// the agent did not send is `false`, never assumed `true` (ADR-0014 §4).
fn parse_agent_capabilities(result: &Value) -> AgentCapabilities {
    let caps = result.get("agentCapabilities");
    let flag = |on: Option<&Value>, key: &str| {
        on.and_then(|v| v.get(key)).and_then(Value::as_bool).unwrap_or(false)
    };
    let prompt = caps.and_then(|c| c.get("promptCapabilities"));
    AgentCapabilities {
        load_session: flag(caps, "loadSession"),
        prompt_capabilities: PromptCapabilities {
            image: flag(prompt, "image"),
            audio: flag(prompt, "audio"),
            embedded_context: flag(prompt, "embeddedContext"),
        },
    }
}

/// The command a tool call would run, as an argv array.
///
/// Never reassembled into a shell string anywhere downstream (ARCHITECTURE.md §9.4, §11).
fn argv_of(tool: Option<&Value>) -> Vec<String> {
    let Some(raw) = tool.and_then(|t| t.get("rawInput")) else { return Vec::new() };
    if let Some(array) = raw.get("command").and_then(Value::as_array) {
        return array.iter().filter_map(Value::as_str).map(str::to_string).collect();
    }
    // Some agents send a single string. Keep it as one argv element rather than splitting
    // on spaces: guessing at quoting is how "rm -rf 'my dir'" becomes two deletions.
    raw.get("command").and_then(Value::as_str).map(|s| vec![s.to_string()]).unwrap_or_default()
}

/// Pick the option matching the human's answer.
fn choose_option(options: &[PermissionOption], decision: PermissionDecision) -> Option<String> {
    let wanted = match decision {
        PermissionDecision::AllowOnce => "allow_once",
        PermissionDecision::AllowAlways => "allow_always",
        PermissionDecision::RejectOnce => "reject_once",
        PermissionDecision::RejectAlways => "reject_always",
    };
    options
        .iter()
        .find(|o| o.kind == wanted)
        .or_else(|| {
            // Fall back within the same direction rather than across it: a missing
            // "always" must never resolve to the opposite answer.
            let fallback = if decision.allows() { "allow_once" } else { "reject_once" };
            options.iter().find(|o| o.kind == fallback)
        })
        .map(|o| o.id.clone())
}

#[cfg(test)]
mod tests {
    /// A cwd the host platform agrees is absolute.
    ///
    /// ACP requires `terminal/create` to carry an absolute cwd, and `valid_absolute_path`
    /// enforces it. `/proj` is rooted but *not* absolute on Windows — that needs a drive
    /// prefix — so hardcoding it made every terminal test fail there while passing on unix.
    const CWD: &str = if cfg!(windows) { r"C:\proj" } else { "/proj" };

    use super::*;

    /// A translator that has completed the handshake and holds one session.
    /// Handshake, then open a session with a caller-supplied `session/new` result.
    fn connected_with(result: Value) -> (Translator, Vec<AgentEvent>) {
        let mut t = Translator::new();
        let init = t.initialize(ClientCapabilities::default());
        let Message::Request { id, .. } = init else { panic!("initialize is a request") };
        t.incoming(Message::Response { id, result: json!({}) });

        let messages = t.outgoing(AgentRequest::NewSession { cwd: "/proj".into() });
        let Message::Request { id, .. } = messages[0].clone() else { panic!() };
        let (events, _) = t.incoming(Message::Response { id, result });
        (t, events)
    }

    fn connected() -> (Translator, SessionId) {
        let mut t = Translator::new();
        let init = t.initialize(ClientCapabilities::default());
        let Message::Request { id, .. } = init else { panic!("initialize is a request") };
        t.incoming(Message::Response { id, result: json!({}) });

        let messages = t.outgoing(AgentRequest::NewSession { cwd: "/proj".into() });
        let Message::Request { id, .. } = messages[0].clone() else { panic!() };
        let (events, _) =
            t.incoming(Message::Response { id, result: json!({ "sessionId": "s-1" }) });
        match events.as_slice() {
            [AgentEvent::SessionStarted { session }] => (t, *session),
            other => panic!("expected a session, got {other:?}"),
        }
    }

    fn update(session: &str, body: Value) -> Message {
        Message::Notification {
            method: "session/update".into(),
            params: json!({ "sessionId": session, "update": body }),
        }
    }

    #[test]
    fn the_handshake_result_is_parsed_rather_than_discarded() {
        let mut t = Translator::new();
        let init = t.initialize(ClientCapabilities::default());
        let Message::Request { id, .. } = init else { panic!("initialize is a request") };
        let (events, _) = t.incoming(Message::Response {
            id,
            result: json!({
                "protocolVersion": 1,
                "agentCapabilities": { "loadSession": true }
            }),
        });
        assert!(
            matches!(
                events.as_slice(),
                [AgentEvent::Ready { capabilities }] if capabilities.load_session
            ),
            "{events:?}"
        );
    }

    #[test]
    fn an_agent_that_says_nothing_is_assumed_to_support_nothing_extra() {
        // Absent means absent. Assuming a capability we were not granted is the
        // failure this parsing exists to prevent (protocol.rs:499-503).
        let mut t = Translator::new();
        let init = t.initialize(ClientCapabilities::default());
        let Message::Request { id, .. } = init else { panic!("initialize is a request") };
        let (events, _) = t.incoming(Message::Response { id, result: json!({}) });
        assert!(
            matches!(
                events.as_slice(),
                [AgentEvent::Ready { capabilities }] if !capabilities.load_session
            ),
            "{events:?}"
        );
    }

    #[test]
    fn queued_requests_still_go_out_after_the_handshake() {
        // Regression: the Initialize arm used to do exactly one useful thing — drain the
        // queue. Parsing the result must not cost us that.
        let mut t = Translator::new();
        let init = t.initialize(ClientCapabilities::default());
        let Message::Request { id, .. } = init else { panic!("initialize is a request") };

        let queued = t.outgoing(AgentRequest::NewSession { cwd: "/proj".into() });
        assert!(queued.is_empty(), "queued before the handshake completes, not sent yet");

        let (_, messages) = t.incoming(Message::Response { id, result: json!({}) });
        assert_eq!(messages.len(), 1, "the queued session/new goes out now: {messages:?}");
        assert!(matches!(&messages[0], Message::Request { method, .. } if method == "session/new"));
    }

    #[test]
    fn initialize_advertises_the_file_capabilities() {
        let mut t = Translator::new();
        let Message::Request { method, params, .. } = t.initialize(ClientCapabilities::default())
        else {
            panic!("initialize is a request")
        };
        assert_eq!(method, "initialize");
        assert_eq!(params["clientCapabilities"]["fs"]["readTextFile"], json!(true));
        assert_eq!(params["clientCapabilities"]["fs"]["writeTextFile"], json!(true));
    }

    #[test]
    fn terminal_capability_is_advertised_only_when_enabled() {
        let mut disabled = Translator::new();
        let Message::Request { params, .. } = disabled.initialize(ClientCapabilities::default())
        else {
            panic!()
        };
        assert_eq!(params["clientCapabilities"]["terminal"], json!(false));

        let mut enabled = Translator::new();
        let Message::Request { params, .. } = enabled
            .initialize(ClientCapabilities { terminal: true, ..ClientCapabilities::default() })
        else {
            panic!()
        };
        assert_eq!(params["clientCapabilities"]["terminal"], json!(true));
    }

    /// A user who starts a session the instant the app opens must not lose it to the
    /// handshake still being in flight.
    #[test]
    fn requests_made_before_the_handshake_completes_are_queued_not_dropped() {
        let mut t = Translator::new();
        let init = t.initialize(ClientCapabilities::default());
        let Message::Request { id, .. } = init else { panic!() };

        assert!(t.outgoing(AgentRequest::NewSession { cwd: "/proj".into() }).is_empty());

        let (_, messages) = t.incoming(Message::Response { id, result: json!({}) });
        assert!(
            matches!(&messages[0], Message::Request { method, .. } if method == "session/new"),
            "the queued request goes out once we are ready, got {messages:?}"
        );
    }

    #[test]
    fn a_session_id_is_ours_not_the_agents_string() {
        let (t, session) = connected();
        assert_eq!(t.wire_session(session), Some("s-1"));
        assert_eq!(t.our_session("s-1"), Some(session));
        assert_eq!(t.our_session("nope"), None);
    }

    #[test]
    fn a_prompt_carries_the_context_before_the_question() {
        let (mut t, session) = connected();
        let messages = t.outgoing(AgentRequest::Prompt {
            session,
            text: "fix it".into(),
            context: "project: proj".into(),
        });
        let Message::Request { params, method, .. } = &messages[0] else { panic!() };
        assert_eq!(method, "session/prompt");
        assert_eq!(params["sessionId"], json!("s-1"));
        assert_eq!(params["prompt"][0]["text"], json!("project: proj"));
        assert_eq!(params["prompt"][1]["text"], json!("fix it"));
    }

    #[test]
    fn streamed_text_and_reasoning_are_distinguished() {
        let (mut t, session) = connected();

        let (events, _) = t.incoming(update(
            "s-1",
            json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": "hi" } }),
        ));
        assert_eq!(events, vec![AgentEvent::MessageChunk { session, text: "hi".into() }]);

        let (events, _) = t.incoming(update(
            "s-1",
            json!({ "sessionUpdate": "agent_thought_chunk", "content": { "type": "text", "text": "hmm" } }),
        ));
        assert_eq!(events, vec![AgentEvent::ThoughtChunk { session, text: "hmm".into() }]);
    }

    /// Finding 2: edits arrive as whole-file diffs inside a tool call.
    #[test]
    fn an_edit_is_lifted_out_of_a_tool_call_diff() {
        let (mut t, session) = connected();
        let (events, _) = t.incoming(update(
            "s-1",
            json!({
                "sessionUpdate": "tool_call",
                "title": "Edit main.rs",
                "content": [{
                    "type": "diff",
                    "path": "/proj/main.rs",
                    "oldText": "fn main() {}\n",
                    "newText": "fn run() {}\n"
                }]
            }),
        ));
        match events.as_slice() {
            [AgentEvent::ProposedEdit { session: s, path, old_text, new_text, .. }] => {
                assert_eq!(*s, session);
                assert_eq!(path, &PathBuf::from("/proj/main.rs"));
                assert_eq!(old_text.as_deref(), Some("fn main() {}\n"));
                assert_eq!(new_text, "fn run() {}\n");
            }
            other => panic!("expected a proposal, got {other:?}"),
        }
    }

    /// Codex opens its session in a read-only mode and offers `auto` and `full-access`
    /// beside it. Parsing the session id and discarding the rest left it permanently
    /// unable to edit, with no way to say so (ADR-0015).
    #[test]
    fn a_session_reports_the_modes_the_agent_offered() {
        let (_, events) = connected_with(json!({
            "sessionId": "s-1",
            "modes": {
                "currentModeId": "read-only",
                "availableModes": [
                    {"id": "read-only", "name": "Read Only", "description": "Can read files."},
                    {"id": "auto", "name": "Default", "description": "Can read and edit."}
                ]
            }
        }));

        let (current, available) = events
            .iter()
            .find_map(|event| match event {
                AgentEvent::ModesAvailable { current, available, .. } => {
                    Some((current.clone(), available.clone()))
                }
                _ => None,
            })
            .expect("the modes reach the client");
        assert_eq!(current, "read-only", "the session starts in the agent's choice");
        assert_eq!(available.len(), 2);
        assert_eq!(available[1].name, "Default");
        assert_eq!(available[1].description.as_deref(), Some("Can read and edit."));
    }

    /// Most agents offer no modes at all, which is not a malformed session.
    #[test]
    fn a_session_without_modes_reports_none() {
        let (_, events) = connected_with(json!({ "sessionId": "s-1" }));
        assert!(
            !events.iter().any(|e| matches!(e, AgentEvent::ModesAvailable { .. })),
            "got {events:?}"
        );
    }

    /// The client's view of the mode follows the agent's report, never its own request.
    #[test]
    fn the_agent_reporting_a_mode_change_is_what_moves_the_client() {
        let (mut t, session) = connected();

        let messages = t.outgoing(AgentRequest::SetMode { session, mode: "auto".into() });
        assert!(
            matches!(&messages[0], Message::Request { method, params, .. }
                if method == "session/set_mode"
                    && params["modeId"] == "auto"
                    && params["sessionId"] == "s-1"),
            "got {messages:?}"
        );

        let (events, _) = t.incoming(update(
            "s-1",
            json!({"sessionUpdate": "current_mode_update", "modeId": "auto"}),
        ));
        assert_eq!(events.as_slice(), [AgentEvent::ModeChanged { session, mode: "auto".into() }]);
    }

    /// codex-acp answers `session/set_mode` with a bare `{}` and never sends
    /// `current_mode_update`. Believing only the notification would strand the pane on
    /// `read-only` for the very agent session modes exist to unblock (ADR-0015 §5).
    #[test]
    fn a_bare_success_is_the_agent_saying_it_changed_the_mode() {
        let (mut t, session) = connected();

        let messages = t.outgoing(AgentRequest::SetMode { session, mode: "auto".into() });
        let Message::Request { id, .. } = messages[0].clone() else { panic!("a request") };

        let (events, _) = t.incoming(Message::Response { id, result: json!({}) });
        assert_eq!(events.as_slice(), [AgentEvent::ModeChanged { session, mode: "auto".into() }]);
    }

    /// The other half of the same rule: an agent that refuses has not changed anything,
    /// so the refusal is reported and the mode is left where the agent still has it.
    #[test]
    fn a_refused_mode_change_reports_the_refusal_and_moves_nothing() {
        let (mut t, session) = connected();

        let messages = t.outgoing(AgentRequest::SetMode { session, mode: "full-access".into() });
        let Message::Request { id, .. } = messages[0].clone() else { panic!("a request") };

        let (events, _) =
            t.incoming(Message::Error { id, code: -32602, message: "unknown mode".into() });
        assert!(
            !events.iter().any(|e| matches!(e, AgentEvent::ModeChanged { .. })),
            "got {events:?}"
        );
        assert!(
            matches!(&events[0], AgentEvent::Failed { session: s, .. } if *s == session),
            "got {events:?}"
        );
    }

    #[test]
    fn a_new_file_has_no_old_text() {
        let (mut t, _) = connected();
        let (events, _) = t.incoming(update(
            "s-1",
            json!({
                "sessionUpdate": "tool_call",
                "content": [{ "type": "diff", "path": "/proj/new.rs", "newText": "hello\n" }]
            }),
        ));
        assert!(matches!(events.as_slice(), [AgentEvent::ProposedEdit { old_text: None, .. }]));
    }

    #[test]
    fn non_diff_tool_content_produces_no_proposal() {
        let (mut t, _) = connected();
        let (events, _) = t.incoming(update(
            "s-1",
            json!({
                "sessionUpdate": "tool_call",
                "content": [{ "type": "content", "content": { "type": "text", "text": "ran it" } }]
            }),
        ));
        assert!(events.is_empty());
    }

    #[test]
    fn updates_for_an_unknown_session_are_ignored() {
        let (mut t, _) = connected();
        let (events, _) = t.incoming(update(
            "someone-else",
            json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": "hi" } }),
        ));
        assert!(events.is_empty(), "not our session, not our problem");
    }

    #[test]
    fn an_unmodelled_update_kind_is_skipped_rather_than_fatal() {
        let (mut t, _) = connected();
        let (events, replies) =
            t.incoming(update("s-1", json!({ "sessionUpdate": "plan", "entries": [] })));
        assert!(events.is_empty() && replies.is_empty(), "spec churn must not break us");
    }

    // --- calls from the agent -------------------------------------------------------

    #[test]
    fn a_file_read_becomes_an_event_and_its_answer_goes_back_to_the_right_id() {
        let (mut t, session) = connected();
        let (events, replies) = t.incoming(Message::Request {
            id: 42,
            method: "fs/read_text_file".into(),
            params: json!({ "sessionId": "s-1", "path": "/proj/main.rs" }),
        });
        assert!(replies.is_empty(), "we answer once the model serves the text");
        let AgentEvent::ReadFileRequested { session: s, request, path } = events[0].clone() else {
            panic!("expected a read, got {events:?}")
        };
        assert_eq!((s, path), (session, PathBuf::from("/proj/main.rs")));

        let out = t.outgoing(AgentRequest::FileContents {
            session,
            request,
            path: "/proj/main.rs".into(),
            contents: Some("live text".into()),
        });
        assert_eq!(
            out,
            vec![Message::Response { id: 42, result: json!({ "content": "live text" }) }]
        );
    }

    /// An agent told a file is empty will happily "fix" it by rewriting it whole.
    #[test]
    fn a_read_we_cannot_serve_is_an_error_not_an_empty_file() {
        let (mut t, session) = connected();
        let (events, _) = t.incoming(Message::Request {
            id: 9,
            method: "fs/read_text_file".into(),
            params: json!({ "sessionId": "s-1", "path": "/proj/gone.rs" }),
        });
        let AgentEvent::ReadFileRequested { request, .. } = events[0].clone() else { panic!() };

        let out = t.outgoing(AgentRequest::FileContents {
            session,
            request,
            path: "/proj/gone.rs".into(),
            contents: None,
        });
        assert!(matches!(out.as_slice(), [Message::Error { id: 9, .. }]), "got {out:?}");
    }

    /// An agent that reads a file twice in one turn — read, edit, re-read to confirm —
    /// must get two answers. Correlating by path would drop one and block it forever.
    #[test]
    fn two_reads_of_the_same_file_are_both_answered() {
        let (mut t, session) = connected();

        let mut requests = Vec::new();
        for wire_id in [10, 11] {
            let (events, _) = t.incoming(Message::Request {
                id: wire_id,
                method: "fs/read_text_file".into(),
                params: json!({ "sessionId": "s-1", "path": "/proj/main.rs" }),
            });
            let AgentEvent::ReadFileRequested { request, .. } = events[0].clone() else {
                panic!("expected a read, got {events:?}")
            };
            requests.push(request);
        }
        assert_ne!(requests[0], requests[1], "each call gets its own id");

        let mut answered = Vec::new();
        for (i, request) in requests.iter().enumerate() {
            let out = t.outgoing(AgentRequest::FileContents {
                session,
                request: *request,
                path: "/proj/main.rs".into(),
                contents: Some(format!("read {i}")),
            });
            match out.as_slice() {
                [Message::Response { id, .. }] => answered.push(*id),
                other => panic!("read {i} went unanswered: {other:?}"),
            }
        }
        assert_eq!(answered, vec![10, 11], "both wire ids answered, in order");
    }

    /// The capability we advertise has to exist. Answering "not supported" to a method we
    /// said we support sends the agent off to write the file itself, unreviewed.
    #[test]
    fn a_write_becomes_a_proposal_and_is_acknowledged() {
        let (mut t, session) = connected();
        let (events, replies) = t.incoming(Message::Request {
            id: 21,
            method: "fs/write_text_file".into(),
            params: json!({
                "sessionId": "s-1",
                "path": "/proj/main.rs",
                "content": "fn run() {}\n"
            }),
        });

        match events.as_slice() {
            [AgentEvent::ProposedEdit { session: s, path, old_text, new_text, .. }] => {
                assert_eq!(*s, session);
                assert_eq!(path, &PathBuf::from("/proj/main.rs"));
                assert_eq!(new_text, "fn run() {}\n");
                assert_eq!(*old_text, None, "the client knows the base; the agent did not say");
            }
            other => panic!("expected a proposal, got {other:?}"),
        }
        assert!(
            matches!(replies.as_slice(), [Message::Response { id: 21, .. }]),
            "the write must be acknowledged, not refused: {replies:?}"
        );
    }

    #[test]
    fn a_malformed_write_is_refused_rather_than_silently_dropped() {
        let (mut t, _) = connected();
        let (events, replies) = t.incoming(Message::Request {
            id: 22,
            method: "fs/write_text_file".into(),
            params: json!({ "sessionId": "s-1", "path": "/proj/main.rs" }),
        });
        assert!(events.is_empty());
        assert!(matches!(replies.as_slice(), [Message::Error { id: 22, .. }]));
    }

    #[test]
    fn a_permission_request_surfaces_the_command_as_argv() {
        let (mut t, _) = connected();
        let (events, _) = t.incoming(Message::Request {
            id: 5,
            method: "session/request_permission".into(),
            params: json!({
                "sessionId": "s-1",
                "toolCall": { "title": "Run tests", "rawInput": { "command": ["cargo", "test"] } },
                "options": [
                    { "optionId": "o1", "kind": "allow_once" },
                    { "optionId": "o2", "kind": "reject_once" }
                ]
            }),
        });
        match events.as_slice() {
            [AgentEvent::PermissionRequested { summary, command, .. }] => {
                assert_eq!(summary, "Run tests");
                assert_eq!(command, &["cargo", "test"]);
            }
            other => panic!("expected a permission request, got {other:?}"),
        }
    }

    #[test]
    fn a_string_command_is_not_split_on_spaces() {
        // Guessing at quoting is how "rm -rf 'my dir'" becomes two deletions.
        let (mut t, _) = connected();
        let (events, _) = t.incoming(Message::Request {
            id: 5,
            method: "session/request_permission".into(),
            params: json!({
                "sessionId": "s-1",
                "toolCall": { "rawInput": { "command": "rm -rf 'my dir'" } },
                "options": []
            }),
        });
        match events.as_slice() {
            [AgentEvent::PermissionRequested { command, terminal_spec, .. }] => {
                assert_eq!(command, &["rm -rf 'my dir'"], "one element, quoting intact");
                assert!(terminal_spec.is_none(), "shell text must never become an exact grant");
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn the_humans_answer_selects_the_matching_option() {
        for (decision, expected) in [
            (PermissionDecision::AllowOnce, "o1"),
            (PermissionDecision::AllowAlways, "o2"),
            (PermissionDecision::RejectOnce, "o3"),
        ] {
            let (mut t, _) = connected();
            let (events, _) = t.incoming(Message::Request {
                id: 5,
                method: "session/request_permission".into(),
                params: json!({
                    "sessionId": "s-1",
                    "toolCall": {},
                    "options": [
                        { "optionId": "o1", "kind": "allow_once" },
                        { "optionId": "o2", "kind": "allow_always" },
                        { "optionId": "o3", "kind": "reject_once" }
                    ]
                }),
            });
            let AgentEvent::PermissionRequested { request, .. } = events[0].clone() else {
                panic!()
            };

            let out = t.outgoing(AgentRequest::Permission { request, decision });
            let Message::Response { result, .. } = &out[0] else { panic!("got {out:?}") };
            assert_eq!(result["outcome"]["optionId"], json!(expected), "for {decision:?}");
        }
    }

    #[test]
    fn cancelling_a_permission_prompt_uses_the_acp_cancelled_outcome() {
        let (mut t, _) = connected();
        let (events, _) = t.incoming(Message::Request {
            id: 5,
            method: "session/request_permission".into(),
            params: json!({
                "sessionId": "s-1",
                "toolCall": {},
                "options": [{ "optionId": "reject", "kind": "reject_once" }]
            }),
        });
        let AgentEvent::PermissionRequested { request, .. } = events[0].clone() else { panic!() };

        let out = t.outgoing(AgentRequest::PermissionCancelled { request });
        let Message::Response { result, .. } = &out[0] else { panic!("got {out:?}") };
        assert_eq!(result, &json!({ "outcome": { "outcome": "cancelled" } }));
    }

    /// A missing "always" must fall back within its own direction, never across it.
    #[test]
    fn a_missing_option_never_flips_the_answer() {
        let (mut t, _) = connected();
        let (events, _) = t.incoming(Message::Request {
            id: 5,
            method: "session/request_permission".into(),
            params: json!({
                "sessionId": "s-1",
                "toolCall": {},
                "options": [{ "optionId": "only-reject", "kind": "reject_once" }]
            }),
        });
        let AgentEvent::PermissionRequested { request, .. } = events[0].clone() else { panic!() };

        let out = t.outgoing(AgentRequest::Permission {
            request,
            decision: PermissionDecision::AllowAlways,
        });
        let Message::Response { result, .. } = &out[0] else { panic!() };
        assert_eq!(
            result["outcome"]["outcome"],
            json!("cancelled"),
            "no allow option offered, so we decline rather than pick a reject"
        );
    }

    #[test]
    fn terminal_create_becomes_a_structured_local_request() {
        let (mut t, session) = connected();
        let (events, replies) = t.incoming(Message::Request {
            id: 11,
            method: "terminal/create".into(),
            params: json!({
                "sessionId": "s-1",
                "command": "cargo",
                "args": ["test"],
                "cwd": CWD,
                "env": [],
                "outputByteLimit": 4096
            }),
        });
        assert!(replies.is_empty());
        assert!(matches!(events.as_slice(), [AgentEvent::TerminalRequest {
            session: owner,
            operation: termesh_core::AgentTerminalOperation::Create {
                spec,
                output_byte_limit: 4096,
                preauthorized: false,
            },
            ..
        }] if *owner == session && spec.program == "cargo" && spec.args == ["test"]));
    }

    #[test]
    fn terminal_ids_correlate_output_wait_kill_and_release() {
        let (mut t, session) = connected();
        let (events, _) = t.incoming(Message::Request {
            id: 20,
            method: "terminal/create".into(),
            params: json!({
                "sessionId": "s-1", "command": "cargo", "args": ["test"],
                "cwd": CWD, "env": []
            }),
        });
        let AgentEvent::TerminalRequest { request, .. } = events[0].clone() else { panic!() };
        let terminal = termesh_core::TerminalId::new(7);
        let created = t.outgoing(AgentRequest::TerminalResponse {
            request,
            response: termesh_core::AgentTerminalResponse::Created { terminal },
        });
        assert_eq!(
            created,
            [Message::Response { id: 20, result: json!({ "terminalId": "termesh-7" }) }]
        );

        let methods = [
            ("terminal/output", termesh_core::AgentTerminalOperation::Output { terminal }),
            (
                "terminal/wait_for_exit",
                termesh_core::AgentTerminalOperation::WaitForExit { terminal },
            ),
            ("terminal/kill", termesh_core::AgentTerminalOperation::Kill { terminal }),
            ("terminal/release", termesh_core::AgentTerminalOperation::Release { terminal }),
        ];
        for (offset, (method, expected)) in methods.into_iter().enumerate() {
            let (events, replies) = t.incoming(Message::Request {
                id: 30 + offset as u64,
                method: method.into(),
                params: json!({ "sessionId": "s-1", "terminalId": "termesh-7" }),
            });
            assert!(replies.is_empty());
            assert!(matches!(events.as_slice(), [AgentEvent::TerminalRequest {
                session: owner,
                operation,
                ..
            }] if *owner == session && *operation == expected));
        }
    }

    #[test]
    fn terminal_responses_use_acp_shapes_and_release_invalidates_operations() {
        let (mut t, session) = connected();
        let (events, _) = t.incoming(Message::Request {
            id: 60,
            method: "terminal/create".into(),
            params: json!({
                "sessionId": "s-1", "command": "cargo", "cwd": CWD, "env": []
            }),
        });
        let AgentEvent::TerminalRequest { request, .. } = events[0].clone() else { panic!() };
        let terminal = TerminalId::new(9);
        let _ = t.outgoing(AgentRequest::TerminalResponse {
            request,
            response: AgentTerminalResponse::Created { terminal },
        });

        let (events, _) = t.incoming(Message::Request {
            id: 61,
            method: "terminal/output".into(),
            params: json!({ "sessionId": "s-1", "terminalId": "termesh-9" }),
        });
        let AgentEvent::TerminalRequest { request, .. } = events[0].clone() else { panic!() };
        assert_eq!(
            t.outgoing(AgentRequest::TerminalResponse {
                request,
                response: AgentTerminalResponse::Output {
                    output: "ok".into(),
                    truncated: false,
                    exit: Some(TerminalExit { code: Some(0), signal: None }),
                },
            }),
            [Message::Response {
                id: 61,
                result: json!({
                    "output": "ok", "truncated": false,
                    "exitStatus": { "exitCode": 0, "signal": null }
                }),
            }]
        );

        let (events, _) = t.incoming(Message::Request {
            id: 62,
            method: "terminal/wait_for_exit".into(),
            params: json!({ "sessionId": "s-1", "terminalId": "termesh-9" }),
        });
        let AgentEvent::TerminalRequest { request, .. } = events[0].clone() else { panic!() };
        assert_eq!(
            t.outgoing(AgentRequest::TerminalResponse {
                request,
                response: AgentTerminalResponse::Exited(TerminalExit {
                    code: None,
                    signal: Some("SIGTERM".into()),
                }),
            }),
            [Message::Response {
                id: 62,
                result: json!({ "exitCode": null, "signal": "SIGTERM" }),
            }]
        );

        let (events, _) = t.incoming(Message::Request {
            id: 63,
            method: "terminal/release".into(),
            params: json!({ "sessionId": "s-1", "terminalId": "termesh-9" }),
        });
        let AgentEvent::TerminalRequest { request, .. } = events[0].clone() else { panic!() };
        assert_eq!(
            t.outgoing(AgentRequest::TerminalResponse {
                request,
                response: AgentTerminalResponse::Acknowledged,
            }),
            [Message::Response { id: 63, result: json!({}) }]
        );

        let (events, replies) = t.incoming(Message::Request {
            id: 64,
            method: "terminal/output".into(),
            params: json!({ "sessionId": "s-1", "terminalId": "termesh-9" }),
        });
        assert!(events.is_empty());
        assert!(matches!(replies.as_slice(), [Message::Error { id: 64, code: -32602, .. }]));

        let (events, _) = t.incoming(update(
            "s-1",
            json!({
                "sessionUpdate": "tool_call",
                "content": [{ "type": "terminal", "terminalId": "termesh-9" }]
            }),
        ));
        assert_eq!(events, [AgentEvent::TerminalAttached { session, terminal }]);
    }

    #[test]
    fn terminal_create_clamps_output_and_validates_structured_fields() {
        let (mut t, _) = connected();
        let (events, _) = t.incoming(Message::Request {
            id: 70,
            method: "terminal/create".into(),
            params: json!({
                "sessionId": "s-1", "command": "env", "args": ["ok"], "cwd": CWD,
                "env": [{ "name": "LANG", "value": "C" }],
                "outputByteLimit": 999999999
            }),
        });
        assert!(matches!(events.as_slice(), [AgentEvent::TerminalRequest {
            operation: AgentTerminalOperation::Create { spec, output_byte_limit: MAX_OUTPUT_LIMIT, .. },
            ..
        }] if spec.env == [("LANG".into(), "C".into())]));

        for params in [
            json!({ "sessionId": "s-1", "command": "x", "cwd": "relative" }),
            json!({ "sessionId": "s-1", "command": "x", "cwd": CWD, "args": [1] }),
            json!({ "sessionId": "s-1", "command": "x", "cwd": CWD, "env": [{}] }),
        ] {
            let (events, replies) =
                t.incoming(Message::Request { id: 71, method: "terminal/create".into(), params });
            assert!(events.is_empty());
            assert!(matches!(replies.as_slice(), [Message::Error { code: -32602, .. }]));
        }
    }

    #[test]
    fn exact_permission_grant_is_consumed_by_one_matching_create() {
        let (mut t, _) = connected();
        let (events, _) = t.incoming(Message::Request {
            id: 40,
            method: "session/request_permission".into(),
            params: json!({
                "sessionId": "s-1",
                "toolCall": { "rawInput": {
                    "command": ["cargo", "test"], "cwd": CWD, "env": []
                }},
                "options": [{ "optionId": "yes", "kind": "allow_once" }]
            }),
        });
        let AgentEvent::PermissionRequested { request, terminal_spec, .. } = events[0].clone()
        else {
            panic!()
        };
        assert!(terminal_spec.is_some());
        let _ = t.outgoing(AgentRequest::Permission {
            request,
            decision: PermissionDecision::AllowOnce,
        });

        for expected in [true, false] {
            let (events, _) = t.incoming(Message::Request {
                id: 41,
                method: "terminal/create".into(),
                params: json!({
                    "sessionId": "s-1", "command": "cargo", "args": ["test"],
                    "cwd": CWD, "env": []
                }),
            });
            assert!(matches!(events.as_slice(), [AgentEvent::TerminalRequest {
                operation: termesh_core::AgentTerminalOperation::Create { preauthorized, .. },
                ..
            }] if *preauthorized == expected));
        }
    }

    /// ADR-0008 §5: an ambiguous grant must never cause an unapproved launch. "Allow
    /// once" is scoped to the turn it was given in — if the agent does not spend it
    /// before the turn ends, it is gone. Otherwise a grant from twenty turns ago silently
    /// preauthorizes a `terminal/create` the user was never asked about.
    #[test]
    fn an_unspent_grant_does_not_survive_the_turn_it_was_given_in() {
        for end_of_turn in [EndOfTurn::Completed, EndOfTurn::Cancelled, EndOfTurn::Failed] {
            let (mut t, session) = connected();
            let (events, _) = t.incoming(Message::Request {
                id: 40,
                method: "session/request_permission".into(),
                params: json!({
                    "sessionId": "s-1",
                    "toolCall": { "rawInput": {
                        "command": ["npm", "install"], "cwd": CWD, "env": []
                    }},
                    "options": [{ "optionId": "yes", "kind": "allow_once" }]
                }),
            });
            let AgentEvent::PermissionRequested { request, .. } = events[0].clone() else {
                panic!("expected a permission request")
            };
            let _ = t.outgoing(AgentRequest::Permission {
                request,
                decision: PermissionDecision::AllowOnce,
            });

            // The agent never creates the terminal; the turn simply ends.
            match end_of_turn {
                EndOfTurn::Completed => {
                    let out = t.outgoing(AgentRequest::Prompt {
                        session,
                        text: "go".into(),
                        context: String::new(),
                    });
                    let Message::Request { id, .. } = out[0].clone() else {
                        panic!("prompt should be a request")
                    };
                    let _ = t.incoming(Message::Response {
                        id,
                        result: json!({ "stopReason": "end_turn" }),
                    });
                }
                EndOfTurn::Cancelled => {
                    let _ = t.outgoing(AgentRequest::Cancel { session });
                }
                // A prompt that errors out ends the turn just as surely as one that
                // completes, and reaches a different arm of the translator.
                EndOfTurn::Failed => {
                    let out = t.outgoing(AgentRequest::Prompt {
                        session,
                        text: "go".into(),
                        context: String::new(),
                    });
                    let Message::Request { id, .. } = out[0].clone() else {
                        panic!("prompt should be a request")
                    };
                    let _ = t.incoming(Message::Error {
                        id,
                        code: -32000,
                        message: "model unavailable".into(),
                    });
                }
            }

            let (events, _) = t.incoming(Message::Request {
                id: 41,
                method: "terminal/create".into(),
                params: json!({
                    "sessionId": "s-1", "command": "npm", "args": ["install"],
                    "cwd": CWD, "env": []
                }),
            });
            assert!(
                matches!(events.as_slice(), [AgentEvent::TerminalRequest {
                    operation: termesh_core::AgentTerminalOperation::Create { preauthorized, .. },
                    ..
                }] if !*preauthorized),
                "{end_of_turn:?}: a stale grant must not preauthorize a launch"
            );
        }
    }

    #[derive(Debug, Clone, Copy)]
    enum EndOfTurn {
        Completed,
        Cancelled,
        Failed,
    }

    #[test]
    fn malformed_terminal_create_receives_an_error() {
        let (mut t, _) = connected();
        let (events, replies) = t.incoming(Message::Request {
            id: 50,
            method: "terminal/create".into(),
            params: json!({ "sessionId": "s-1", "command": "", "cwd": "relative" }),
        });
        assert!(events.is_empty());
        assert!(matches!(replies.as_slice(), [Message::Error { id: 50, code: -32602, .. }]));
    }

    #[test]
    fn terminal_ids_are_owned_by_the_session_that_created_them() {
        let (mut t, _) = connected();
        let (events, _) = t.incoming(Message::Request {
            id: 80,
            method: "terminal/create".into(),
            params: json!({
                "sessionId": "s-1", "command": "cargo", "cwd": CWD, "env": []
            }),
        });
        let AgentEvent::TerminalRequest { request, .. } = events[0].clone() else { panic!() };
        let _ = t.outgoing(AgentRequest::TerminalResponse {
            request,
            response: AgentTerminalResponse::Created { terminal: TerminalId::new(12) },
        });

        let messages = t.outgoing(AgentRequest::NewSession { cwd: "/proj".into() });
        let Message::Request { id, .. } = messages[0] else { panic!() };
        let _ = t.incoming(Message::Response { id, result: json!({ "sessionId": "s-2" }) });

        let (events, replies) = t.incoming(Message::Request {
            id: 81,
            method: "terminal/output".into(),
            params: json!({ "sessionId": "s-2", "terminalId": "termesh-12" }),
        });
        assert!(events.is_empty());
        assert!(matches!(replies.as_slice(), [Message::Error { id: 81, code: -32602, .. }]));
    }

    // --- turn lifecycle --------------------------------------------------------------

    #[test]
    fn a_prompt_response_ends_the_turn_with_its_reason() {
        for (wire, expected) in [
            ("end_turn", StopReason::EndTurn),
            ("cancelled", StopReason::Cancelled),
            ("refusal", StopReason::Refusal),
            ("max_tokens", StopReason::MaxTokens),
        ] {
            let (mut t, session) = connected();
            let out = t.outgoing(AgentRequest::Prompt {
                session,
                text: "go".into(),
                context: String::new(),
            });
            let Message::Request { id, .. } = out[0].clone() else { panic!() };

            let (events, _) =
                t.incoming(Message::Response { id, result: json!({ "stopReason": wire }) });
            assert_eq!(events, vec![AgentEvent::TurnEnded { session, reason: expected }]);
        }
    }

    #[test]
    fn an_error_response_to_a_prompt_fails_that_session() {
        let (mut t, session) = connected();
        let out =
            t.outgoing(AgentRequest::Prompt { session, text: "go".into(), context: String::new() });
        let Message::Request { id, .. } = out[0].clone() else { panic!() };

        let (events, _) =
            t.incoming(Message::Error { id, code: -32000, message: "model unavailable".into() });
        assert_eq!(
            events,
            vec![AgentEvent::Failed { session, message: "model unavailable".into() }]
        );
    }

    #[test]
    fn cancelling_is_a_notification_not_a_request() {
        let (mut t, session) = connected();
        let out = t.outgoing(AgentRequest::Cancel { session });
        assert!(
            matches!(&out[0], Message::Notification { method, .. } if method == "session/cancel"),
            "got {out:?}"
        );
    }

    #[test]
    fn a_response_we_are_not_waiting_on_is_ignored() {
        let (mut t, _) = connected();
        let (events, replies) = t.incoming(Message::Response { id: 9999, result: json!({}) });
        assert!(events.is_empty() && replies.is_empty());
    }
}
