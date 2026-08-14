//! Clipboard, shell, paths, OS integration. Phase 01+.
//!
//! See ARCHITECTURE.md §7 for how this crate fits the workspace.
#![forbid(unsafe_code)]

pub mod clipboard;
pub mod color;
pub mod paths;
pub mod shell;

pub use clipboard::{ClipboardError, ClipboardService, Osc52Clipboard};
pub use color::{current_color_depth, detect_color_depth, ColorDepth};
pub use paths::{
    agents_file, config_dir, config_file, drafts_dir, keymap_file, session_file, APP_DIR,
};
pub use shell::{default_shell, human_command, shell};
