//! The `AgentService` boundary — every ACP wire type stays behind this (ADR-0007).
//!
//! Synchronous and object-safe, following the template ADR-0005 §3 set for
//! `FileSystemService`: the methods block, but they are only ever called from the agent
//! worker thread, so the non-blocking guarantee comes from *where* they run. That is what
//! lets a scripted agent be a plain struct with a queue — no executor, no runtime — and
//! it is why `tokio` is still not in the tree.

pub use termesh_core::agent::{AgentEvent, AgentRequest, PermissionDecision, StopReason};

/// How the agent is wired into the workspace (ADR-0003).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentIntegration {
    /// Tier 0: run an AI CLI inside a terminal pane. No shared state, but free.
    TerminalCli,
    /// Tier 1: native ACP client with shared context and inline diff review.
    Acp,
}

/// What the client offers to do on the agent's behalf (ADR-0007 §3).
///
/// These are advertised at `initialize`, and the defaults are a deliberate product
/// decision rather than a shrug — see [`Default`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClientCapabilities {
    /// Serve file contents from the live buffer, unsaved changes included. This is the
    /// concrete mechanism behind "the agent shares your buffers" (ARCHITECTURE.md §9.2).
    pub read_text_file: bool,
    /// Accept writes — as *proposals*, never straight to disk.
    pub write_text_file: bool,
    /// Execute structured commands through model-owned PTY terminals.
    pub terminal: bool,
}

impl Default for ClientCapabilities {
    /// Both on.
    ///
    /// Advertising `write_text_file: false` is tempting and wrong: an agent told the
    /// client cannot write files does not give up, it shells out and writes the file
    /// itself, turning a reviewable proposal into an opaque side effect. Saying yes and
    /// routing every write through review is what *keeps* edits in the loop.
    fn default() -> Self {
        Self { read_text_file: true, write_text_file: true, terminal: false }
    }
}

/// The human's verdict while reviewing a proposal. Per hunk *and* per proposal, as
/// ARCHITECTURE.md §9.3 requires.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProposalDecision {
    AcceptAll,
    AcceptHunk(usize),
    RejectHunk(usize),
    RejectAll,
}

/// What to tell the agent after a review, given how much of it the human took.
///
/// ACP has no `AllowPartial`, so a partial accept is answered `RejectOnce` (ADR-0007 §8).
/// That looks wrong and is right: from the agent's side its write did not happen *as
/// proposed*, and claiming otherwise would leave it building on a file it believes
/// matches `new_text`. The follow-up message and a fresh read resync it to the truth.
pub fn permission_for_review(accepted: usize, total: usize) -> PermissionDecision {
    if total > 0 && accepted == total {
        PermissionDecision::AllowOnce
    } else {
        PermissionDecision::RejectOnce
    }
}

/// The ACP client surface.
///
/// Behind a trait so the wire format is isolated (ADR-0003's spec-churn mitigation), so a
/// non-ACP backend could be substituted, and — most usefully day to day — so the whole
/// review loop is testable against a scripted agent replaying a recorded stream.
pub trait AgentService: Send {
    fn integration(&self) -> AgentIntegration;

    fn capabilities(&self) -> ClientCapabilities {
        ClientCapabilities::default()
    }

    /// Queue work for the agent. Never blocks the caller.
    fn send(&mut self, request: AgentRequest);

    /// Take whatever the agent has produced since the last call.
    ///
    /// A drain rather than a callback so the single state owner stays in control of when
    /// events are applied (ARCHITECTURE.md §7.1).
    fn poll(&mut self) -> Vec<AgentEvent>;
}

/// The default when no agent is configured: Tier 0, and honest about it.
///
/// ADR-0003 promises agent-agnosticism, which we keep in the *default* and not just in
/// the abstraction — no vendor is assumed, and the editor works with none configured.
#[derive(Debug, Default)]
pub struct NullAgent;

impl AgentService for NullAgent {
    fn integration(&self) -> AgentIntegration {
        AgentIntegration::TerminalCli
    }
    fn send(&mut self, _request: AgentRequest) {}
    fn poll(&mut self) -> Vec<AgentEvent> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_tier_needs_no_configured_agent() {
        assert_eq!(NullAgent.integration(), AgentIntegration::TerminalCli);
        assert!(NullAgent.poll().is_empty());
    }

    #[test]
    fn we_advertise_both_file_capabilities() {
        let caps = ClientCapabilities::default();
        assert!(caps.read_text_file, "serving live buffers is the whole point");
        assert!(caps.write_text_file, "saying no just pushes the agent to shell out");
        assert!(!caps.terminal, "terminal support is enabled only after runtime wiring");
    }

    /// ADR-0007 §8 — the one place our UX is not expressible in the protocol.
    #[test]
    fn a_partial_accept_is_reported_as_a_rejection() {
        assert_eq!(permission_for_review(3, 3), PermissionDecision::AllowOnce);
        assert_eq!(
            permission_for_review(2, 3),
            PermissionDecision::RejectOnce,
            "the agent must not think the file matches what it proposed"
        );
        assert_eq!(permission_for_review(0, 3), PermissionDecision::RejectOnce);
    }

    #[test]
    fn an_empty_proposal_is_not_an_approval() {
        assert_eq!(permission_for_review(0, 0), PermissionDecision::RejectOnce);
    }
}
