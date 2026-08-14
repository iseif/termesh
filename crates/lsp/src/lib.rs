//! Language-server processes + JSON-RPC client. Phase 07.
//!
//! See ARCHITECTURE.md §7 for how this crate fits the workspace.
#![forbid(unsafe_code)]

pub mod framing;
pub mod jsonrpc;
pub mod protocol;
pub mod recipe;
pub mod service;
pub mod session;

pub use framing::{encode_frame, FrameReader};
pub use jsonrpc::{DecodeError, Message, RequestIds};
pub use protocol::Translator;
pub use recipe::{missing_server, recipe_for, resolve_recipe, server_available, Recipe};
pub use service::{LanguageService, NullLanguageService};
pub use session::LspSession;
