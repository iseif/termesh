//! Backend-agnostic keyboard input. The `app` crate translates crossterm events
//! into these types so the keymap (`config`) never depends on the terminal backend.
use core::fmt;

/// A logical key, independent of any terminal library.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Key {
    Char(char),
    Enter,
    Esc,
    Tab,
    BackTab,
    Backspace,
    Delete,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    F(u8),
}

/// Modifier state for a chord.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Mods {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
}

impl Mods {
    pub const NONE: Mods = Mods { ctrl: false, alt: false, shift: false };
    pub const CTRL: Mods = Mods { ctrl: true, alt: false, shift: false };
    pub const CTRL_SHIFT: Mods = Mods { ctrl: true, alt: false, shift: true };
    pub const ALT: Mods = Mods { ctrl: false, alt: true, shift: false };
    pub const SHIFT: Mods = Mods { ctrl: false, alt: false, shift: true };
    pub fn is_none(self) -> bool {
        !self.ctrl && !self.alt && !self.shift
    }
}

/// Where a binding applies.
///
/// Phase 02 got away with a focus check inside the command handler, because nothing
/// competed for the arrow keys. The editor is the second consumer — `Down` means "next
/// tree row" in the explorer and "next line" in a buffer — so the choice belongs in
/// resolution, where one chord can mean different things in different panes, rather than
/// in a growing pile of `if focus != ...` guards.
///
/// Resolution tries the focused context first and falls back to [`KeyContext::Global`],
/// so `Ctrl+S` keeps working everywhere while `Enter` is free to differ.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum KeyContext {
    /// Applies regardless of focus.
    Global,
    /// Only while the file explorer has focus.
    Project,
    /// Only while a buffer has focus.
    Editor,
    /// Only while the agent pane has focus and a session exists.
    Agent,
    /// Only while terminal copy mode is active. Normal terminal input bypasses the
    /// keymap and is encoded directly for the PTY (ADR-0008 §3).
    Terminal,
}

/// A key plus its modifiers — the unit a keymap binds to a [`crate::Command`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct KeyChord {
    pub key: Key,
    pub mods: Mods,
}

impl KeyChord {
    pub const fn new(key: Key, mods: Mods) -> Self {
        Self { key, mods }
    }
    pub const fn plain(key: Key) -> Self {
        Self { key, mods: Mods::NONE }
    }
    pub const fn ctrl(key: Key) -> Self {
        Self { key, mods: Mods::CTRL }
    }
    pub const fn ctrl_shift(key: Key) -> Self {
        Self { key, mods: Mods::CTRL_SHIFT }
    }
    pub const fn alt(key: Key) -> Self {
        Self { key, mods: Mods::ALT }
    }
    pub const fn shift(key: Key) -> Self {
        Self { key, mods: Mods::SHIFT }
    }
}

impl KeyChord {
    /// Whether a legacy terminal can even deliver this chord distinguishably.
    ///
    /// Without the kitty keyboard protocol — which we do not enable — `Ctrl+<key>` is
    /// sent as the control byte `key & 0x1f`, and several of those bytes are already
    /// spoken for by named keys or by each other:
    ///
    /// | chord        | byte | indistinguishable from       |
    /// |--------------|------|------------------------------|
    /// | `Ctrl+I`     | 0x09 | `Tab`                        |
    /// | `Ctrl+M`     | 0x0D | `Enter`                      |
    /// | `Ctrl+J`     | 0x0A | `Enter` (line feed)          |
    /// | `Ctrl+H`     | 0x08 | `Backspace`                  |
    /// | `Ctrl+[`     | 0x1B | `Esc`                        |
    /// | ``Ctrl+` ``  | 0x00 | `Ctrl+@`, `Ctrl+Space` (NUL) |
    /// | `Ctrl+@`     | 0x00 | ``Ctrl+` ``, `Ctrl+Space`    |
    /// | `Ctrl+Space` | 0x00 | ``Ctrl+` ``, `Ctrl+@`        |
    /// | `Ctrl+Shift+letter` | same control byte as `Ctrl+letter` | shift is lost |
    ///
    /// The NUL family is worse than merely ambiguous: most emulators decline to send
    /// anything at all for ``Ctrl+` ``, and macOS claims `Ctrl+Space` for input-source
    /// switching before any terminal sees it. Neither is reachable in practice.
    ///
    /// Binding one of these does not fail loudly — it silently does whatever the *other*
    /// key is bound to, or nothing, which is indistinguishable from the feature being
    /// broken. So the default keymap is tested against this rather than trusted.
    pub fn is_terminal_ambiguous(&self) -> bool {
        if !self.mods.ctrl || self.mods.alt {
            return false;
        }
        if self.mods.shift && matches!(self.key, Key::Char(_)) {
            return true;
        }
        matches!(self.key, Key::Char('i' | 'm' | 'j' | 'h' | '[' | '`' | '@' | ' '))
    }
}

impl fmt::Display for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Key::Char(' ') => write!(f, "Space"),
            Key::Char(c) => write!(f, "{}", c.to_ascii_uppercase()),
            Key::Enter => write!(f, "Enter"),
            Key::Esc => write!(f, "Esc"),
            Key::Tab => write!(f, "Tab"),
            Key::BackTab => write!(f, "Shift+Tab"),
            Key::Backspace => write!(f, "Backspace"),
            Key::Delete => write!(f, "Del"),
            Key::Up => write!(f, "\u{2191}"),
            Key::Down => write!(f, "\u{2193}"),
            Key::Left => write!(f, "\u{2190}"),
            Key::Right => write!(f, "\u{2192}"),
            Key::Home => write!(f, "Home"),
            Key::End => write!(f, "End"),
            Key::PageUp => write!(f, "PgUp"),
            Key::PageDown => write!(f, "PgDn"),
            Key::F(n) => write!(f, "F{n}"),
        }
    }
}

impl fmt::Display for KeyChord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.mods.ctrl {
            write!(f, "Ctrl+")?;
        }
        if self.mods.alt {
            write!(f, "Alt+")?;
        }
        if self.mods.shift && !matches!(self.key, Key::BackTab) {
            write!(f, "Shift+")?;
        }
        write!(f, "{}", self.key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_terminals_cannot_distinguish_ctrl_shift_letters_from_ctrl_letters() {
        for letter in ['a', 'f', 'p', 'z'] {
            assert!(KeyChord::ctrl_shift(Key::Char(letter)).is_terminal_ambiguous());
        }
        assert!(!KeyChord::plain(Key::F(9)).is_terminal_ambiguous());
        assert!(!KeyChord::plain(Key::F(10)).is_terminal_ambiguous());
    }
}
