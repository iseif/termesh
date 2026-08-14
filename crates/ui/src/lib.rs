//! Layout engine, theme tokens, focus regions, and stateless widgets. Phase 01.
//! See ARCHITECTURE.md §6 (UX) and §7 (architecture).
#![forbid(unsafe_code)]

pub mod layout;
pub mod overlays;
pub mod text;
pub mod theme;
pub mod widgets;

pub use layout::{centered_rect, cursor_anchored_rect, regions, LayoutState, Pane, Regions};
pub use text::{display_column, display_width, expand_tabs};
pub use theme::Theme;
