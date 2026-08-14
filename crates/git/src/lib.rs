//! Repository status model + git backend (CLI first). Phase 06.
//!
//! See ARCHITECTURE.md §7 for how this crate fits the workspace.
#![forbid(unsafe_code)]

mod diff;
mod real;
mod service;
mod status;
mod worker;

pub use diff::{bounded_context_diff, bounded_diff};
pub use real::RealGitService;
pub use service::GitService;
pub use status::parse_status;
pub use worker::GitWorker;
