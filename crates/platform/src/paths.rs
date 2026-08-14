//! Where our files live on each OS (ARCHITECTURE.md §13).
//!
//! Resolved from environment variables rather than via the `dirs` crate: it is a dozen
//! lines, and a dependency added this early would outlive the reason for it.
//!
//! The directory name comes from one constant, which is what made the 0.1.0 rename off
//! the `termide` codename a single-line change here (ADR-0004).

use std::path::PathBuf;

/// The application name used for config/state directories.
pub const APP_DIR: &str = "termesh";

/// `~/.config/<app>/` on Unix, `%APPDATA%\<app>\` on Windows.
///
/// Returns `None` when the environment gives us nothing usable — running without a home
/// directory is unusual but legitimate (containers, CI), and should degrade to "no
/// persistence" rather than fail.
pub fn config_dir() -> Option<PathBuf> {
    if cfg!(windows) {
        std::env::var_os("APPDATA").map(|p| PathBuf::from(p).join(APP_DIR))
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
            .map(|base| base.join(APP_DIR))
    }
}

/// The file recording recent workspaces and the last session.
pub fn session_file() -> Option<PathBuf> {
    config_dir().map(|d| d.join("session.toml"))
}

/// Where ACP agent definitions live (ARCHITECTURE.md §13).
pub fn agents_file() -> Option<PathBuf> {
    config_dir().map(|d| d.join("agents.toml"))
}

/// The user's global settings (ARCHITECTURE.md §13).
pub fn config_file() -> Option<PathBuf> {
    config_dir().map(|d| d.join("config.toml"))
}

/// The user's keybinding overrides (ARCHITECTURE.md §13).
pub fn keymap_file() -> Option<PathBuf> {
    config_dir().map(|d| d.join("keymap.toml"))
}

/// Crash-recovery drafts, one file per dirty buffer (ADR-0014 §5).
pub fn drafts_dir() -> Option<PathBuf> {
    config_dir().map(|directory| directory.join("drafts"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_session_file_lives_under_the_config_dir() {
        // Only assert the relationship: the absolute location depends on the
        // environment, and asserting that would just re-implement the function.
        if let (Some(dir), Some(file)) = (config_dir(), session_file()) {
            assert!(file.starts_with(&dir));
            assert_eq!(file.file_name().unwrap(), "session.toml");
        }
    }

    #[test]
    fn the_config_file_lives_under_the_config_dir() {
        if let (Some(dir), Some(file)) = (config_dir(), config_file()) {
            assert!(file.starts_with(&dir));
            assert_eq!(file.file_name().unwrap(), "config.toml");
        }
    }

    #[test]
    fn the_keymap_file_lives_under_the_config_dir() {
        if let (Some(dir), Some(file)) = (config_dir(), keymap_file()) {
            assert!(file.starts_with(&dir));
            assert_eq!(file.file_name().unwrap(), "keymap.toml");
        }
    }

    #[test]
    fn the_drafts_directory_lives_under_the_config_dir() {
        if let (Some(dir), Some(drafts)) = (config_dir(), drafts_dir()) {
            assert!(drafts.starts_with(&dir));
            assert_eq!(drafts.file_name().unwrap(), "drafts");
        }
    }

    #[test]
    fn the_config_dir_is_namespaced_by_the_app_name() {
        if let Some(dir) = config_dir() {
            assert_eq!(dir.file_name().unwrap(), APP_DIR);
        }
    }
}
