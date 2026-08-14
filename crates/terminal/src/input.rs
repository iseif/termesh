//! Encode backend-neutral key chords as terminal input bytes (ADR-0008 §3).

use termesh_core::input::{Key, KeyChord, Mods};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct InputModes {
    pub application_cursor: bool,
}

pub fn encode_key(chord: KeyChord, modes: InputModes) -> Option<Vec<u8>> {
    if let Key::Char(c) = chord.key {
        return encode_char(c, chord.mods);
    }

    if chord.key == Key::Tab && chord.mods == Mods::SHIFT {
        return Some(b"\x1b[Z".to_vec());
    }

    let modifier = modifier_parameter(chord.mods);
    let bytes = match chord.key {
        Key::Char(_) => unreachable!("characters are handled above"),
        Key::Enter if chord.mods.is_none() => b"\r".as_slice(),
        Key::Esc if chord.mods.is_none() => b"\x1b".as_slice(),
        Key::Tab if chord.mods.is_none() => b"\t".as_slice(),
        Key::BackTab if chord.mods.is_none() => b"\x1b[Z".as_slice(),
        Key::Backspace if chord.mods.is_none() => b"\x7f".as_slice(),
        Key::Up if chord.mods.is_none() && modes.application_cursor => b"\x1bOA".as_slice(),
        Key::Down if chord.mods.is_none() && modes.application_cursor => b"\x1bOB".as_slice(),
        Key::Right if chord.mods.is_none() && modes.application_cursor => b"\x1bOC".as_slice(),
        Key::Left if chord.mods.is_none() && modes.application_cursor => b"\x1bOD".as_slice(),
        Key::Home if chord.mods.is_none() && modes.application_cursor => b"\x1bOH".as_slice(),
        Key::End if chord.mods.is_none() && modes.application_cursor => b"\x1bOF".as_slice(),
        Key::Up if chord.mods.is_none() => b"\x1b[A".as_slice(),
        Key::Down if chord.mods.is_none() => b"\x1b[B".as_slice(),
        Key::Right if chord.mods.is_none() => b"\x1b[C".as_slice(),
        Key::Left if chord.mods.is_none() => b"\x1b[D".as_slice(),
        Key::Home if chord.mods.is_none() => b"\x1b[H".as_slice(),
        Key::End if chord.mods.is_none() => b"\x1b[F".as_slice(),
        Key::PageUp if chord.mods.is_none() => b"\x1b[5~".as_slice(),
        Key::PageDown if chord.mods.is_none() => b"\x1b[6~".as_slice(),
        Key::Delete if chord.mods.is_none() => b"\x1b[3~".as_slice(),
        Key::F(1) if chord.mods.is_none() => b"\x1bOP".as_slice(),
        Key::F(2) if chord.mods.is_none() => b"\x1bOQ".as_slice(),
        Key::F(3) if chord.mods.is_none() => b"\x1bOR".as_slice(),
        Key::F(4) if chord.mods.is_none() => b"\x1bOS".as_slice(),
        Key::F(5) if chord.mods.is_none() => b"\x1b[15~".as_slice(),
        Key::F(6) if chord.mods.is_none() => b"\x1b[17~".as_slice(),
        Key::F(7) if chord.mods.is_none() => b"\x1b[18~".as_slice(),
        Key::F(8) if chord.mods.is_none() => b"\x1b[19~".as_slice(),
        Key::F(9) if chord.mods.is_none() => b"\x1b[20~".as_slice(),
        Key::F(10) if chord.mods.is_none() => b"\x1b[21~".as_slice(),
        Key::F(11) if chord.mods.is_none() => b"\x1b[23~".as_slice(),
        Key::F(12) if chord.mods.is_none() => b"\x1b[24~".as_slice(),
        Key::Up | Key::Down | Key::Right | Key::Left | Key::Home | Key::End
            if modifier.is_some() =>
        {
            return encode_modified_navigation(chord.key, modifier.expect("checked above"));
        }
        Key::PageUp | Key::PageDown | Key::Delete if modifier.is_some() => {
            return encode_modified_tilde(chord.key, modifier.expect("checked above"));
        }
        _ => return None,
    };
    Some(bytes.to_vec())
}

fn encode_char(c: char, mods: Mods) -> Option<Vec<u8>> {
    if mods.ctrl {
        if !c.is_ascii_alphabetic() {
            return None;
        }
        let control = (c.to_ascii_uppercase() as u8) & 0x1f;
        let mut bytes = Vec::with_capacity(2);
        if mods.alt {
            bytes.push(0x1b);
        }
        bytes.push(control);
        return Some(bytes);
    }

    let mut bytes = Vec::with_capacity(c.len_utf8() + usize::from(mods.alt));
    if mods.alt {
        bytes.push(0x1b);
    }
    let mut encoded = [0; 4];
    bytes.extend_from_slice(c.encode_utf8(&mut encoded).as_bytes());
    Some(bytes)
}

fn modifier_parameter(mods: Mods) -> Option<u8> {
    if mods.is_none() {
        return None;
    }
    Some(1 + u8::from(mods.shift) + 2 * u8::from(mods.alt) + 4 * u8::from(mods.ctrl))
}

fn encode_modified_navigation(key: Key, modifier: u8) -> Option<Vec<u8>> {
    let final_byte = match key {
        Key::Up => 'A',
        Key::Down => 'B',
        Key::Right => 'C',
        Key::Left => 'D',
        Key::Home => 'H',
        Key::End => 'F',
        _ => return None,
    };
    Some(format!("\x1b[1;{modifier}{final_byte}").into_bytes())
}

fn encode_modified_tilde(key: Key, modifier: u8) -> Option<Vec<u8>> {
    let number = match key {
        Key::PageUp => 5,
        Key::PageDown => 6,
        Key::Delete => 3,
        _ => return None,
    };
    Some(format!("\x1b[{number};{modifier}~").into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_c_is_one_control_byte() {
        let bytes = encode_key(KeyChord::ctrl(Key::Char('c')), InputModes::default());
        assert_eq!(bytes, Some(vec![0x03]));
    }

    #[test]
    fn application_cursor_up_uses_ss3() {
        let modes = InputModes { application_cursor: true };
        assert_eq!(encode_key(KeyChord::plain(Key::Up), modes), Some(b"\x1bOA".to_vec()));
    }

    #[test]
    fn printable_unicode_stays_utf8_and_alt_adds_escape() {
        assert_eq!(
            encode_key(KeyChord::plain(Key::Char('λ')), InputModes::default()),
            Some("λ".as_bytes().to_vec())
        );
        assert_eq!(
            encode_key(KeyChord::alt(Key::Char('x')), InputModes::default()),
            Some(b"\x1bx".to_vec())
        );
    }

    #[test]
    fn named_keys_use_terminal_sequences() {
        for (chord, expected) in [
            (KeyChord::plain(Key::Enter), b"\r".as_slice()),
            (KeyChord::plain(Key::Backspace), b"\x7f".as_slice()),
            (KeyChord::plain(Key::BackTab), b"\x1b[Z".as_slice()),
            (KeyChord::plain(Key::Delete), b"\x1b[3~".as_slice()),
            (KeyChord::plain(Key::F(12)), b"\x1b[24~".as_slice()),
        ] {
            assert_eq!(encode_key(chord, InputModes::default()), Some(expected.to_vec()));
        }
    }

    #[test]
    fn unsupported_function_keys_are_dropped() {
        assert_eq!(encode_key(KeyChord::plain(Key::F(13)), InputModes::default()), None);
    }
}
