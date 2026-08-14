//! Global settings — `~/.config/<app>/config.toml` (ARCHITECTURE.md §13).
//!
//! Mirrors [`crate::agents`]'s discipline: parsed from `&str` so the caller reads the
//! bytes through `FileSystemService`, and never fatal — there is no configuration file
//! shape that should stop the editor from starting (ADR-0014 §3).

use serde::Deserialize;
use std::path::PathBuf;

/// The keys this struct actually understands, for the unknown-key sweep in [`Settings::parse`].
const KNOWN_KEYS: &[&str] =
    &["version", "theme", "shell", "tab_width", "soft_wrap", "autosave", "exclusions"];

/// Keys that parse and round-trip but have no consumer wired up yet. Update this list —
/// never the message text — as each one gains one (ADR-0014 §1, §3).
const NOT_YET_APPLIED: &[&str] = &["soft_wrap"];

const MIN_TAB_WIDTH: u8 = 1;
const MAX_TAB_WIDTH: u8 = 16;

/// Which compiled palette to use. A single variant today — the token layer
/// (`crates/ui/src/theme.rs`) is what `~/.config/<app>/themes/` will read from once it
/// lands in Phase 11 (ADR-0014 §1).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ThemeChoice {
    #[default]
    Dark,
}

/// How eagerly a dirty buffer is mirrored to a crash-recovery draft (Task 10). Not a
/// save-to-disk interval — drafts never touch the file the human owns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Autosave {
    Off,
    Debounced { seconds: u32 },
}

impl Default for Autosave {
    fn default() -> Self {
        Autosave::Debounced { seconds: 2 }
    }
}

/// The user's global settings.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub version: u32,
    pub theme: ThemeChoice,
    pub shell: Option<String>,
    pub tab_width: u8,
    pub soft_wrap: bool,
    pub autosave: Autosave,
    pub exclusions: Vec<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            version: 1,
            theme: ThemeChoice::default(),
            shell: None,
            tab_width: 4,
            soft_wrap: true,
            autosave: Autosave::default(),
            exclusions: Vec::new(),
        }
    }
}

/// What a configuration file got wrong, and what we did about it. Surfaced in-app per
/// ARCHITECTURE.md §13: file, line, explanation, fallback taken.
///
/// `file` is left empty by [`Settings::parse`], which only sees the file's *contents* —
/// the caller, who read the bytes and knows the path, fills it in before display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigDiagnostic {
    pub file: PathBuf,
    pub line: Option<u32>,
    pub problem: String,
    pub fallback: String,
}

impl ConfigDiagnostic {
    pub(crate) fn new(
        line: Option<u32>,
        problem: impl Into<String>,
        fallback: impl Into<String>,
    ) -> Self {
        Self { file: PathBuf::new(), line, problem: problem.into(), fallback: fallback.into() }
    }
}

/// 1-based line number of the byte offset `at`, for matching what the user's editor shows.
pub(crate) fn line_of(text: &str, at: usize) -> u32 {
    text.get(..at.min(text.len())).unwrap_or(text).matches('\n').count() as u32 + 1
}

impl Settings {
    /// The schema version this build writes and fully understands (ADR-0014 §2). A file
    /// naming an older version is migrated in memory on read, never rewritten just for
    /// migrating. A file naming a newer one loads what it recognizes and says what it did
    /// not — a beta binary cannot know the next binary's shape.
    pub const CURRENT_VERSION: u32 = 1;

    /// The parsed settings only, discarding diagnostics — for callers that already know
    /// the input is well-formed, like [`crate::migrate`]'s idempotency test.
    pub fn parse_raw(text: &str) -> Settings {
        Self::parse(text).0
    }

    /// Never fails. A file that cannot be parsed at all yields the compiled defaults
    /// plus one diagnostic naming the line and the fallback taken (ADR-0014 §3).
    pub fn parse(text: &str) -> (Settings, Vec<ConfigDiagnostic>) {
        let mut problems = Vec::new();

        let mut settings: Settings = match toml::from_str(text) {
            Ok(settings) => settings,
            Err(error) => {
                let line = error.span().map(|span| line_of(text, span.start));
                problems.push(ConfigDiagnostic::new(
                    line,
                    error.message().to_string(),
                    "using default settings",
                ));
                return (Settings::default(), problems);
            }
        };

        // Deserializing into the typed struct silently drops keys it does not name;
        // walk the raw table separately so a typo is reported rather than swallowed.
        if let Ok(toml::Value::Table(table)) = toml::from_str::<toml::Value>(text) {
            for key in table.keys() {
                if !KNOWN_KEYS.contains(&key.as_str()) {
                    problems.push(ConfigDiagnostic::new(
                        None,
                        format!("unknown key '{key}'"),
                        "ignoring it",
                    ));
                }
            }
            // Remove a key here (and its test below) the moment its consumer lands —
            // `soft_wrap` in Phase 11 — or a working setting
            // keeps telling the user it does nothing yet.
            for key in NOT_YET_APPLIED {
                if table.contains_key(*key) {
                    problems.push(ConfigDiagnostic::new(
                        None,
                        format!("'{key}' is recognized but not yet applied"),
                        "value is stored; not yet applied in this build",
                    ));
                }
            }
        }

        if settings.version > Self::CURRENT_VERSION {
            problems.push(ConfigDiagnostic::new(
                None,
                format!(
                    "version {} is newer than this build understands (current: {})",
                    settings.version,
                    Self::CURRENT_VERSION
                ),
                "loaded what was understood; newer keys may not have been applied",
            ));
        } else if settings.version < Self::CURRENT_VERSION {
            settings = crate::migrate::migrate(settings);
        }

        if !(MIN_TAB_WIDTH..=MAX_TAB_WIDTH).contains(&settings.tab_width) {
            let original = settings.tab_width;
            settings.tab_width = settings.tab_width.clamp(MIN_TAB_WIDTH, MAX_TAB_WIDTH);
            problems.push(ConfigDiagnostic::new(
                None,
                format!("tab_width {original} is out of range ({MIN_TAB_WIDTH}-{MAX_TAB_WIDTH})"),
                format!("clamped to {}", settings.tab_width),
            ));
        }

        (settings, problems)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_file_yields_defaults() {
        let (settings, problems) = Settings::parse("");
        assert_eq!(settings, Settings::default());
        assert!(problems.is_empty());
    }

    #[test]
    fn a_valid_file_overrides_only_what_it_names() {
        let (settings, problems) = Settings::parse("version = 1\ntab_width = 2\n");
        assert_eq!(settings.tab_width, 2);
        assert_eq!(settings.soft_wrap, Settings::default().soft_wrap, "the rest is untouched");
        assert!(problems.is_empty());
    }

    #[test]
    fn a_file_with_no_version_is_treated_as_version_one() {
        // The first schema predates the key that names it.
        let (settings, problems) = Settings::parse("tab_width = 2\n");
        assert_eq!(settings.tab_width, 2);
        assert!(problems.is_empty());
    }

    #[test]
    fn a_file_from_the_future_loads_what_it_understands_and_says_what_it_did_not() {
        let text = format!("version = {}\ntab_width = 2\n", Settings::CURRENT_VERSION + 1);
        let (settings, problems) = Settings::parse(&text);
        assert_eq!(settings.tab_width, 2, "a newer file's known keys still load");
        assert_eq!(problems.len(), 1);
        assert!(problems[0].problem.contains("newer"));
        assert!(problems[0].fallback.contains("understood"));
    }

    #[test]
    fn a_malformed_file_yields_defaults_and_names_the_line() {
        let (settings, problems) = Settings::parse("version = 1\ntab_width = \n");
        assert_eq!(settings, Settings::default(), "an unusable file must not disable the editor");
        let problem = &problems[0];
        assert_eq!(problem.line, Some(2));
        assert!(problem.fallback.contains("default"), "§13: say which fallback was taken");
    }

    #[test]
    fn an_unknown_key_is_reported_and_the_rest_still_loads() {
        // Dropping a key silently is how a user spends an hour on a typo.
        let (settings, problems) = Settings::parse("version = 1\ntab_width = 2\nteb_width = 8\n");
        assert_eq!(settings.tab_width, 2);
        assert_eq!(problems.len(), 1);
        assert!(problems[0].problem.contains("teb_width"));
    }

    #[test]
    fn an_out_of_range_value_is_clamped_and_reported() {
        let (settings, problems) = Settings::parse("version = 1\ntab_width = 200\n");
        assert!(settings.tab_width <= 16);
        assert_eq!(problems.len(), 1);
    }

    #[test]
    fn the_compiled_theme_is_an_applied_setting() {
        let (settings, problems) = Settings::parse("version = 1\ntheme = \"dark\"\n");
        assert_eq!(settings.theme, ThemeChoice::Dark);
        assert!(problems.is_empty(), "Task 7 gives the compiled palette a real consumer");
    }

    #[test]
    fn autosave_is_an_applied_draft_debounce_setting() {
        let (settings, problems) =
            Settings::parse("version = 1\nautosave = { debounced = { seconds = 3 } }\n");
        assert_eq!(settings.autosave, Autosave::Debounced { seconds: 3 });
        assert!(problems.is_empty(), "Task 10 consumes autosave: {problems:#?}");
    }

    #[test]
    fn soft_wrap_is_accepted_but_reported_as_not_yet_applied() {
        // Deferred to Phase 11 (ADR-0014 §1) — not a Task 3 slice, see the ADR for why.
        let (settings, problems) = Settings::parse("version = 1\nsoft_wrap = false\n");
        assert!(!settings.soft_wrap);
        assert_eq!(problems.len(), 1);
        assert!(problems[0].problem.contains("soft_wrap"));
        assert!(problems[0].fallback.contains("not yet applied"));
    }
}
