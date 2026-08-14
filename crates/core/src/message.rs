//! Typed application messages — the one channel the main loop wakes on (ARCHITECTURE.md §7.1).
//!
//! Every off-loop producer funnels into this enum: the terminal input pump, the
//! filesystem worker, and the PTY, search, task, git, LSP, and ACP streams. The loop
//! blocks on the *channel*, never on any single source, which is what lets a file-watch
//! event repaint the tree without the user touching the keyboard (ADR-0005 §1).
//!
//! Backend-agnostic on purpose: crossterm types are translated in `app` before they get
//! here, so `core` stays free of any terminal backend.

use crate::agent::AgentEvent;
use crate::fs::FsEvent;
use crate::git::GitEvent;
use crate::input::KeyChord;
use crate::terminal::PtyEvent;
use crate::SearchEvent;
use crate::{LspEvent, LspServerId};

/// A message from an off-loop producer to the single owner of application state.
///
/// Exhaustive on purpose: adding a producer should break every loop that has not
/// decided what to do about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppMessage {
    /// A resolved key chord from the terminal input pump.
    Input(KeyChord),
    /// The terminal was resized. Ratatui re-measures on the next draw, so this carries
    /// no dimensions — it exists to wake the loop so that draw happens.
    Resize,
    /// A result from the filesystem worker: a directory listing, a failure, or a
    /// watch notification. The reason the loop can no longer block on the keyboard.
    Fs(FsEvent),
    /// A streamed workspace-search update.
    Search(SearchEvent),
    /// A result from the serialized Git worker.
    Git(GitEvent),
    /// A language-server event tagged with the session that produced it.
    Lsp(LspServerId, LspEvent),
    /// Output or lifecycle state from the PTY worker.
    Pty(PtyEvent),
    /// Something the agent produced — streamed text, an edit proposal, a permission
    /// request.
    ///
    /// The agent worker feeds this channel exactly as the filesystem worker does, so the
    /// scripted agent and the real ACP client reach the model through the same function.
    /// A fake that took a different path would be testing a route the product never uses.
    Agent(AgentEvent),
}
