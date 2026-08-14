//! User keybindings — `~/.config/<app>/keymap.toml` (ARCHITECTURE.md §13).
//!
//! ```toml
//! version = 1
//!
//! [global]
//! "alt+g" = "git.show"
//!
//! [editor]
//! "alt+g" = "lsp.format"
//! ```
//!
//! One table per [`KeyContext`], each mapping a chord string to an [`Action::id`]. Applied
//! as an overlay onto [`crate::default_keymap`] — a rebind of one chord costs the user
//! nothing else, and a bad line costs one binding, never the file (ADR-0014 §3).

use std::collections::BTreeMap;

use serde::Deserialize;
use termesh_core::input::{Key, KeyChord, KeyContext, Mods};
use termesh_core::{ActionRegistry, Command};

use crate::settings::line_of;
use crate::{ConfigDiagnostic, Keymap};

/// The schema version this build writes and fully understands (ADR-0014 §2), mirroring
/// [`crate::settings::Settings::CURRENT_VERSION`]. There are no transitions yet, so an
/// older file needs no migration — only a newer one needs a diagnostic.
const CURRENT_VERSION: u32 = 1;

const KNOWN_TOP_LEVEL_KEYS: &[&str] =
    &["version", "global", "project", "editor", "terminal", "agent"];

#[derive(Debug, Deserialize)]
#[serde(default)]
struct KeymapFile {
    version: u32,
    global: BTreeMap<String, String>,
    project: BTreeMap<String, String>,
    editor: BTreeMap<String, String>,
    terminal: BTreeMap<String, String>,
    agent: BTreeMap<String, String>,
}

impl Default for KeymapFile {
    fn default() -> Self {
        Self {
            // Absent, like a missing `config.toml` version, predates the key that names
            // it — not "older than version 1", which does not exist.
            version: CURRENT_VERSION,
            global: BTreeMap::new(),
            project: BTreeMap::new(),
            editor: BTreeMap::new(),
            terminal: BTreeMap::new(),
            agent: BTreeMap::new(),
        }
    }
}

/// Overlay `text` onto `keymap` in place. Every rejected line produces one
/// [`ConfigDiagnostic`] and is skipped; a file that does not parse as TOML at all leaves
/// `keymap` completely untouched.
pub fn apply_keymap_file(keymap: &mut Keymap, text: &str) -> Vec<ConfigDiagnostic> {
    let mut problems = Vec::new();

    let file: KeymapFile = match toml::from_str(text) {
        Ok(file) => file,
        Err(error) => {
            let line = error.span().map(|span| line_of(text, span.start));
            problems.push(ConfigDiagnostic::new(
                line,
                error.message().to_string(),
                "keeping the default keymap",
            ));
            return problems;
        }
    };

    // Typed deserialization deliberately accepts unknown fields so a future file can
    // still load what this build understands. Sweep the raw table as well so a typo in
    // a context name is reported rather than disappearing with all of its bindings.
    if let Ok(toml::Value::Table(table)) = toml::from_str::<toml::Value>(text) {
        for key in table.keys() {
            if !KNOWN_TOP_LEVEL_KEYS.contains(&key.as_str()) {
                problems.push(ConfigDiagnostic::new(
                    None,
                    format!("unknown key or context '{key}'"),
                    "ignoring it and keeping the remaining bindings",
                ));
            }
        }
    }

    if file.version > CURRENT_VERSION {
        problems.push(ConfigDiagnostic::new(
            None,
            format!(
                "version {} is newer than this build understands (current: {CURRENT_VERSION})",
                file.version
            ),
            "loaded what was understood; newer bindings may not have been applied",
        ));
    }

    let registry = ActionRegistry::with_defaults();
    let tables = [
        (KeyContext::Global, &file.global),
        (KeyContext::Project, &file.project),
        (KeyContext::Editor, &file.editor),
        (KeyContext::Terminal, &file.terminal),
        (KeyContext::Agent, &file.agent),
    ];

    for (context, table) in tables {
        for (chord_text, action_id) in table {
            let chord = match parse_chord(chord_text) {
                Ok(chord) => chord,
                Err(reason) => {
                    problems.push(ConfigDiagnostic::new(
                        None,
                        format!("'{chord_text}': {reason}"),
                        "skipping this binding",
                    ));
                    continue;
                }
            };
            if chord.is_terminal_ambiguous() {
                problems.push(ConfigDiagnostic::new(
                    None,
                    format!("'{chord_text}' cannot be delivered by a terminal"),
                    "skipping this binding",
                ));
                continue;
            }
            let Some(action) = registry.actions().iter().find(|a| a.id() == action_id) else {
                problems.push(ConfigDiagnostic::new(
                    None,
                    format!("unknown action id '{action_id}'"),
                    "skipping this binding",
                ));
                continue;
            };
            keymap.bind_in(context, chord, Command::Action(action.clone()));
        }
    }

    problems
}

/// `"ctrl+shift+p"`, `"alt+enter"`, `"f12"`, `"shift+pageup"` — modifiers in any order,
/// case-insensitive, joined by `+`, ending in one key name. [`render_chord`] is this
/// grammar's inverse.
fn parse_chord(text: &str) -> Result<KeyChord, String> {
    let mut parts: Vec<&str> = text.split('+').collect();
    let Some(key_part) = parts.pop() else { return Err("empty chord".into()) };
    if key_part.is_empty() {
        return Err(format!("'{text}' names no key"));
    }

    let mut mods = Mods::NONE;
    for part in parts {
        match part.to_ascii_lowercase().as_str() {
            "ctrl" => mods.ctrl = true,
            "alt" => mods.alt = true,
            "shift" => mods.shift = true,
            other => return Err(format!("unknown modifier '{other}'")),
        }
    }

    let key = parse_key(key_part)?;
    Ok(KeyChord::new(key, mods))
}

fn parse_key(text: &str) -> Result<Key, String> {
    let lower = text.to_ascii_lowercase();
    let named = match lower.as_str() {
        "enter" => Some(Key::Enter),
        "esc" | "escape" => Some(Key::Esc),
        "tab" => Some(Key::Tab),
        "backtab" => Some(Key::BackTab),
        "backspace" => Some(Key::Backspace),
        "delete" | "del" => Some(Key::Delete),
        "up" => Some(Key::Up),
        "down" => Some(Key::Down),
        "left" => Some(Key::Left),
        "right" => Some(Key::Right),
        "home" => Some(Key::Home),
        "end" => Some(Key::End),
        "pageup" => Some(Key::PageUp),
        "pagedown" => Some(Key::PageDown),
        "space" => Some(Key::Char(' ')),
        _ => None,
    };
    if let Some(key) = named {
        return Ok(key);
    }
    // "f" followed by digits is a function key; a bare "f" falls through to the
    // single-char case below, exactly like every other letter.
    if let Some(digits) = lower.strip_prefix('f') {
        if let Ok(n) = digits.parse::<u8>() {
            return Ok(Key::F(n));
        }
    }
    // Never lowercased here: `Key::Char('a')` and `Key::Char('A')` are two different
    // chords in this scheme (ARCHITECTURE.md §6.3's `AgentProposalAccept`/`AllowAlways`
    // pair is exactly `a` vs `A`, not `a` vs `Shift+a` — a terminal sends the literal
    // capital, not a modifier bit, for a shifted printable key).
    let mut chars = text.chars();
    let c = chars.next().ok_or_else(|| format!("'{text}' names no key"))?;
    if chars.next().is_some() {
        return Err(format!("unknown key '{text}'"));
    }
    Ok(Key::Char(c))
}

/// The inverse of [`parse_chord`]. Exists for the round-trip property test — nothing in
/// the load path needs to render a chord back to text.
#[cfg(test)]
fn render_chord(chord: &KeyChord) -> String {
    let mut out = String::new();
    if chord.mods.ctrl {
        out.push_str("ctrl+");
    }
    if chord.mods.alt {
        out.push_str("alt+");
    }
    if chord.mods.shift {
        out.push_str("shift+");
    }
    out.push_str(&render_key(chord.key));
    out
}

#[cfg(test)]
fn render_key(key: Key) -> String {
    match key {
        Key::Char(' ') => "space".to_string(),
        Key::Char(c) => c.to_string(),
        Key::Enter => "enter".to_string(),
        Key::Esc => "esc".to_string(),
        Key::Tab => "tab".to_string(),
        Key::BackTab => "backtab".to_string(),
        Key::Backspace => "backspace".to_string(),
        Key::Delete => "delete".to_string(),
        Key::Up => "up".to_string(),
        Key::Down => "down".to_string(),
        Key::Left => "left".to_string(),
        Key::Right => "right".to_string(),
        Key::Home => "home".to_string(),
        Key::End => "end".to_string(),
        Key::PageUp => "pageup".to_string(),
        Key::PageDown => "pagedown".to_string(),
        Key::F(n) => format!("f{n}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use termesh_core::Action;

    #[test]
    fn a_keymap_file_overlays_the_defaults() {
        let mut k = crate::default_keymap();
        let problems =
            apply_keymap_file(&mut k, "version = 1\n\n[global]\n\"alt+g\" = \"git.show\"\n");
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(
            k.resolve(&KeyChord::alt(Key::Char('g')), KeyContext::Global),
            Some(&Command::Action(Action::GitShow)),
        );
        assert_eq!(
            k.resolve(&KeyChord::plain(Key::F(10)), KeyContext::Global),
            Some(&Command::OpenPalette),
            "rebinding one chord does not cost the user the other forty-six",
        );
    }

    #[test]
    fn a_binding_in_a_pane_context_shadows_only_that_pane() {
        let mut k = crate::default_keymap();
        apply_keymap_file(&mut k, "version = 1\n\n[editor]\n\"alt+g\" = \"lsp.format\"\n");
        assert_eq!(
            k.resolve(&KeyChord::alt(Key::Char('g')), KeyContext::Editor),
            Some(&Command::Action(Action::LspFormat))
        );
        assert_eq!(k.resolve(&KeyChord::alt(Key::Char('g')), KeyContext::Project), None);
    }

    #[test]
    fn an_unknown_action_id_is_reported_and_skipped() {
        let mut k = crate::default_keymap();
        let problems =
            apply_keymap_file(&mut k, "version = 1\n\n[global]\n\"alt+g\" = \"git.shwo\"\n");
        assert_eq!(problems.len(), 1);
        assert!(problems[0].problem.contains("git.shwo"));
    }

    #[test]
    fn an_unknown_context_is_reported_instead_of_silently_ignored() {
        // A misspelled table is the keymap equivalent of a misspelled setting. If it
        // disappears during typed deserialization, the user gets no explanation for
        // why every binding in the table did nothing.
        let mut k = crate::default_keymap();
        let problems =
            apply_keymap_file(&mut k, "version = 1\n\n[glboal]\n\"alt+g\" = \"git.show\"\n");
        assert_eq!(problems.len(), 1);
        assert!(problems[0].problem.contains("glboal"));
        assert!(problems[0].fallback.contains("ignoring"));
    }

    #[test]
    fn an_unparseable_chord_is_reported_and_skipped() {
        let mut k = crate::default_keymap();
        let problems =
            apply_keymap_file(&mut k, "version = 1\n\n[global]\n\"hyper+q\" = \"git.show\"\n");
        assert_eq!(problems.len(), 1);
    }

    #[test]
    fn a_user_cannot_bind_a_chord_the_terminal_cannot_deliver() {
        // The Phase-01 guard covers our defaults. A user's file needs the same guard,
        // or they bind ctrl+i and file a bug that Tab stopped working.
        let mut k = crate::default_keymap();
        let problems =
            apply_keymap_file(&mut k, "version = 1\n\n[global]\n\"ctrl+i\" = \"git.show\"\n");
        assert_eq!(problems.len(), 1);
        assert!(problems[0].problem.contains("cannot be delivered"));
    }

    #[test]
    fn a_keymap_file_from_the_future_still_applies_what_it_understands() {
        let mut k = crate::default_keymap();
        let text =
            format!("version = {}\n\n[global]\n\"alt+g\" = \"git.show\"\n", CURRENT_VERSION + 1);
        let problems = apply_keymap_file(&mut k, &text);
        assert_eq!(problems.len(), 1);
        assert!(problems[0].problem.contains("newer"));
        assert_eq!(
            k.resolve(&KeyChord::alt(Key::Char('g')), KeyContext::Global),
            Some(&Command::Action(Action::GitShow)),
            "a newer file's known bindings still load"
        );
    }

    #[test]
    fn a_malformed_file_leaves_the_defaults_completely_intact() {
        let mut k = crate::default_keymap();
        let problems = apply_keymap_file(&mut k, "version = 1\n[global\n");
        assert!(!problems.is_empty());
        assert_eq!(
            k.resolve(&KeyChord::plain(Key::F(10)), KeyContext::Global),
            Some(&Command::OpenPalette)
        );
    }

    #[test]
    fn every_default_chord_round_trips_through_the_grammar() {
        for (_, chord, _) in crate::default_keymap().bindings() {
            let text = render_chord(&chord);
            let parsed = parse_chord(&text).unwrap_or_else(|e| panic!("{text}: {e}"));
            assert_eq!(parsed, chord, "{text} round-tripped to a different chord");
        }
    }

    #[test]
    fn modifiers_parse_case_insensitively_and_in_any_order() {
        assert_eq!(parse_chord("CTRL+SHIFT+p").unwrap(), KeyChord::ctrl_shift(Key::Char('p')));
        assert_eq!(parse_chord("shift+ctrl+p").unwrap(), KeyChord::ctrl_shift(Key::Char('p')));
    }

    #[test]
    fn a_shifted_letter_is_the_capital_not_a_modifier() {
        // Matches the default keymap's own AgentProposalAccept ('a') vs
        // AgentAllowAlways ('A') pair: a terminal sends the literal capital.
        assert_eq!(parse_chord("A").unwrap(), KeyChord::plain(Key::Char('A')));
    }
}
