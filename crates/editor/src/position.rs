//! Char offsets in, protocol positions out.
//!
//! The editor counts chars (ADR-0006 §1); the language protocol counts UTF-16 code
//! units within a line. Conversion lives here because this crate owns the rope, which
//! keeps `termesh-lsp` rope-free and trading only in `TextPosition`.

use ropey::Rope;

/// `(line, character)` for a char offset, with `character` in UTF-16 code units.
pub fn utf16_position(text: &Rope, offset: usize) -> (u32, u32) {
    let offset = offset.min(text.len_chars());
    let line = text.char_to_line(offset);
    let line_start = text.line_to_char(line);
    let character = text.line(line).char_to_utf16_cu(offset - line_start);
    (line as u32, character as u32)
}

/// The char offset for a protocol position, clamped to the line and to the document.
/// Servers do send out-of-range positions; clamping is the contract, not a bug.
pub fn offset_from_utf16(text: &Rope, line: u32, character: u32) -> usize {
    let line = line as usize;
    if line >= text.len_lines() {
        return text.len_chars();
    }

    let line_slice = text.line(line);
    let content_len =
        line_slice.len_chars().saturating_sub(usize::from(line_slice.chars().last() == Some('\n')));
    let content = line_slice.slice(..content_len);
    let character = (character as usize).min(content.len_utf16_cu());
    text.line_to_char(line) + content.utf16_cu_to_char(character)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ropey::Rope;

    #[test]
    fn ascii_positions_round_trip() {
        let text = Rope::from_str("fn main() {\n    let x = 1;\n}\n");
        let offset = 16; // inside the second line
        let (line, character) = utf16_position(&text, offset);
        assert_eq!((line, character), (1, 4));
        assert_eq!(offset_from_utf16(&text, line, character), offset);
    }

    #[test]
    fn a_non_bmp_char_counts_as_two_utf16_units() {
        // The surrogate-pair case is the one naive conversion gets wrong: one char,
        // four UTF-8 bytes, two UTF-16 code units.
        let text = Rope::from_str("let s = \"🦀\";\n");
        let after_crab = text.line(0).chars().position(|c| c == '"').unwrap() + 2;
        let (line, character) = utf16_position(&text, after_crab);
        assert_eq!(line, 0);
        assert_eq!(character, 11, "🦀 must count as two UTF-16 units, not one");
        assert_eq!(offset_from_utf16(&text, line, character), after_crab);
    }

    #[test]
    fn multibyte_bmp_chars_count_as_one_unit() {
        let text = Rope::from_str("// café\n");
        let end = text.line(0).len_chars() - 1;
        let (_, character) = utf16_position(&text, end);
        assert_eq!(character, end as u32);
    }

    #[test]
    fn a_position_past_the_line_clamps_to_the_line_end() {
        let text = Rope::from_str("ab\ncd\n");
        assert_eq!(offset_from_utf16(&text, 0, 99), 2);
    }

    #[test]
    fn a_position_past_the_document_clamps_to_the_end() {
        let text = Rope::from_str("ab\n");
        assert_eq!(offset_from_utf16(&text, 99, 0), text.len_chars());
    }

    #[test]
    fn crlf_normalised_text_uses_buffer_offsets_not_disk_offsets() {
        // Buffer::from_text normalises CRLF to LF, so positions are computed against
        // the normalised text. Sending disk bytes instead would shift every position.
        let text = Rope::from_str("a\nb\n");
        let (line, character) = utf16_position(&text, 2);
        assert_eq!((line, character), (1, 0));
    }
}
