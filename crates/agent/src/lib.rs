//! The agent layer — the project's wedge (ARCHITECTURE.md §9).
//!
//! We implement the **client** side of the Agent Client Protocol (ACP): an open JSON-RPC
//! 2.0 standard where the agent runs as a subprocess over stdio and the editor is the
//! client. Any ACP agent — Claude Code, Codex, Gemini CLI, OpenCode, Goose — plugs in, so
//! we never marry one model vendor. Spec: <https://agentclientprotocol.com>.
//!
//! Integration is **tiered** (ADR-0003), which de-risks the whole bet:
//! - **Tier 0 — [`AgentIntegration::TerminalCli`]**: free. The user runs any AI CLI
//!   inside a managed terminal pane. Ships as soon as the terminal exists (Phase 04),
//!   with zero agent-specific code. If ACP work slips, the product is still useful.
//! - **Tier 1 — [`AgentIntegration::Acp`]**: the differentiator. Full ACP client with
//!   shared project context, inline diff-review of proposed edits, and permission-gated
//!   tool calls.
//!
//! **What this crate does not export matters as much as what it does.** No ACP wire type
//! crosses this boundary: `editor` and `ui` see [`AgentEvent`], [`Hunk`], and
//! [`EditProposal`], never a `SessionUpdate` or a `ToolCallContent`. That isolation is
//! ADR-0003's mitigation for spec churn, and the churn is not hypothetical — the upstream
//! SDK is at 2.0 with an unstable protocol-v2 feature already in flight.
//!
//! Two surfaces live here:
//! - [`service`] — the [`AgentService`] trait, the requests we send, the events we get.
//! - [`proposal`] — turning the agent's whole-file diffs into reviewable hunks, and
//!   carrying those hunks onto a buffer the human has kept typing in.
#![forbid(unsafe_code)]

pub mod acp;
pub mod jsonrpc;
pub mod proposal;
pub mod protocol;
pub mod service;

pub use acp::AcpAgent;
pub use proposal::{
    changeset_from_hunks, hunks_from_diff, rebase_hunks, whole_file_from_permission_diff,
    AnchorFailure, EditProposal, Hunk,
};
pub use service::{
    permission_for_review, AgentEvent, AgentIntegration, AgentRequest, AgentService,
    ClientCapabilities, NullAgent, PermissionDecision, ProposalDecision, StopReason,
};
