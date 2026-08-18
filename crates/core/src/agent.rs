//! Agent data types shared across the service boundary.
//!
//! These live in `core` rather than in `agent` for the same reason [`crate::fs`] does:
//! [`crate::AppMessage`] has to carry them from the agent worker thread to the single
//! state owner (ARCHITECTURE.md §7.1). The `AgentService` trait and its implementations
//! stay in `agent`, which re-exports everything here so call sites see one module.
//!
//! Note what is *not* here: no ACP wire type. These are our own vocabulary, translated at
//! the transport boundary, which is ADR-0003's mitigation for protocol churn.

use std::path::PathBuf;

use crate::{
    AgentTerminalOperation, AgentTerminalRequestId, AgentTerminalResponse, PermissionRequestId,
    ProposalId, ReadRequestId, SessionId, TerminalId, TerminalSpec,
};

/// The protocol's four permission responses.
///
/// Four, not three: the Phase-00 stub's `AllowOnce | AllowSession | Deny` could not
/// express `RejectAlways`, which ACP requires us to round-trip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionDecision {
    AllowOnce,
    AllowAlways,
    RejectOnce,
    RejectAlways,
}

impl PermissionDecision {
    pub fn allows(self) -> bool {
        matches!(self, PermissionDecision::AllowOnce | PermissionDecision::AllowAlways)
    }

    /// Whether this answer should be recorded as a standing policy for the workspace.
    pub fn is_remembered(self) -> bool {
        matches!(self, PermissionDecision::AllowAlways | PermissionDecision::RejectAlways)
    }
}

/// What the agent told us it can do, read from the `initialize` result
/// (ADR-0014 §4). Absent means absent — an agent that says nothing about a capability is
/// assumed **not** to support it, never assumed to. Recorded and reported this phase;
/// Phase 11 gates behaviour on it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AgentCapabilities {
    /// Whether the agent supports `session/load` — resuming a session by id rather than
    /// only ever starting a fresh one. No agent this client has spoken to advertises it
    /// today, and this client has no `session/load` request to send even if one did
    /// (ADR-0014 §4): recorded so the boundary is a measured fact, not an assumption.
    pub load_session: bool,
    pub prompt_capabilities: PromptCapabilities,
}

/// The content kinds a `session/prompt` turn may include, per the agent's own handshake.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PromptCapabilities {
    pub image: bool,
    pub audio: bool,
    pub embedded_context: bool,
}

/// Why a turn ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    EndTurn,
    Cancelled,
    Refusal,
    MaxTokens,
}

/// Work sent *to* the agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentRequest {
    NewSession {
        cwd: PathBuf,
    },
    /// Move the session to one of the modes the agent offered (ADR-0015).
    ///
    /// Only ever sent because a human asked for it. The agent's default stands until
    /// then, including when that default forbids the edit the agent was just asked to
    /// make — a client that widens its own permissions on refusal is not asking.
    SetMode {
        session: SessionId,
        mode: String,
    },
    /// A user turn. `context` is the workspace snapshot rendered as text and prepended —
    /// small and current, because everything bulky is *pulled* on demand instead
    /// (ADR-0007 §4).
    Prompt {
        session: SessionId,
        text: String,
        context: String,
    },
    /// Our answer to [`AgentEvent::ReadFileRequested`], served from the live buffer.
    /// `None` means we could not read it.
    ///
    /// Carries the `request` it answers rather than only the path, so two reads of the
    /// same file in one turn cannot be confused for each other.
    FileContents {
        session: SessionId,
        request: ReadRequestId,
        path: PathBuf,
        contents: Option<String>,
    },
    Permission {
        request: PermissionRequestId,
        decision: PermissionDecision,
    },
    /// Cancel a permission prompt because its owning turn/session is no longer live.
    PermissionCancelled {
        request: PermissionRequestId,
    },
    TerminalResponse {
        request: AgentTerminalRequestId,
        response: AgentTerminalResponse,
    },
    Cancel {
        session: SessionId,
    },
    Shutdown,
}

/// The before-and-after text of an edit an agent is asking permission to make.
///
/// `old_text` is what the agent believes it is replacing. It is **not** reliably the whole
/// file: opencode sends the entire document, Codex sends only the lines it touches, and both
/// arrive in the same `content[]` entry. Deciding which one came is the caller's job, because
/// only the caller holds the buffer to compare against (ADR-0016 §1a).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProposedEditDiff {
    pub path: PathBuf,
    pub old_text: String,
    pub new_text: String,
}

/// One entry from an agent's `availableModes`.
///
/// `description` is the agent's own wording for what the mode permits, which is the only
/// trustworthy account of it: `auto` and `full-access` mean whatever that agent decided,
/// and the client must not infer permissions from the name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionMode {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
}

/// What comes back *from* the agent.
///
/// Exhaustive on purpose, like `FsEvent`: adding a variant should break every loop that
/// has not decided what to do about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentEvent {
    /// The handshake completed. Emitted once, before any session, so it carries no
    /// `SessionId` — capabilities belong to the agent connection, not to a session.
    Ready {
        capabilities: AgentCapabilities,
    },
    SessionStarted {
        session: SessionId,
    },
    /// What the agent will let this session do, and which of those it started in
    /// (ADR-0015). Absent for agents that do not offer modes, which is most of them.
    ModesAvailable {
        session: SessionId,
        current: String,
        available: Vec<SessionMode>,
    },
    /// The agent reports the session is now in `mode`. The agent's account is the truth,
    /// so this is what updates the client — not the response to our own request.
    ModeChanged {
        session: SessionId,
        mode: String,
    },
    /// Streamed assistant text.
    MessageChunk {
        session: SessionId,
        text: String,
    },
    /// Streamed reasoning, rendered dimmer than the answer.
    ThoughtChunk {
        session: SessionId,
        text: String,
    },
    /// The agent asked us for a file. We answer from the buffer if it is open.
    ReadFileRequested {
        session: SessionId,
        request: ReadRequestId,
        path: PathBuf,
    },
    /// A proposed edit, as whole-file before/after text — the shape ACP actually uses.
    /// `old_text` is `None` when the agent is creating a new file.
    ProposedEdit {
        session: SessionId,
        proposal: ProposalId,
        path: PathBuf,
        old_text: Option<String>,
        new_text: String,
    },
    /// A tool call awaiting approval. `command` is an argv array — we never interpolate
    /// agent output into a shell string (ARCHITECTURE.md §9.4, §11).
    PermissionRequested {
        session: SessionId,
        request: PermissionRequestId,
        summary: String,
        command: Vec<String>,
        /// Present only when raw ACP input supplied an exact structured command.
        terminal_spec: Option<TerminalSpec>,
        /// The edit this permission would authorise, when the agent described one.
        ///
        /// An agent that asks before editing is an agent whose edits can be reviewed, so
        /// this is what turns an "allow?" prompt into a diff (ADR-0016 §1).
        edit: Option<ProposedEditDiff>,
    },
    TerminalRequest {
        session: SessionId,
        request: AgentTerminalRequestId,
        operation: AgentTerminalOperation,
    },
    TerminalAttached {
        session: SessionId,
        terminal: TerminalId,
    },
    TurnEnded {
        session: SessionId,
        reason: StopReason,
    },
    Failed {
        session: SessionId,
        message: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permission_answers_split_into_allow_and_remember() {
        assert!(PermissionDecision::AllowOnce.allows());
        assert!(PermissionDecision::AllowAlways.allows());
        assert!(!PermissionDecision::RejectOnce.allows());
        assert!(!PermissionDecision::RejectAlways.allows());

        assert!(!PermissionDecision::AllowOnce.is_remembered());
        assert!(PermissionDecision::AllowAlways.is_remembered());
        assert!(
            PermissionDecision::RejectAlways.is_remembered(),
            "the variant the three-way stub could not express"
        );
    }
}
