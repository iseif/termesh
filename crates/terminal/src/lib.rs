//! PTY sessions + VT parser/grid (alacritty_terminal). Phase 04.
//!
//! See ARCHITECTURE.md §7 for how this crate fits the workspace.
#![forbid(unsafe_code)]

pub mod capture;
pub mod input;
pub mod real;
pub mod screen;
pub mod service;
pub mod worker;

pub use capture::{CapturedOutput, DEFAULT_CAPTURE_LIMIT, MAX_CAPTURE_LIMIT};
pub use input::{encode_key, InputModes};
pub use real::RealPtyService;
pub use screen::{
    ScreenAttributes, ScreenCell, ScreenColor, ScreenCursor, ScreenSnapshot, TerminalScreen,
};
pub use service::{PtyError, PtyEventSink, PtyResult, PtyService};
pub use worker::PtyWorker;
