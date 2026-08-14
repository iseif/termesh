//! Cursor motion over a rope. Pure functions on `(text, position)`, so every rule here
//! is testable without constructing a [`crate::Buffer`].
//!
//! **Char-wise, not grapheme-wise, for now.** ADR-0006 §1 puts grapheme awareness at this
//! layer — one arrow press should cross a whole cluster, so an emoji with a skin-tone
//! modifier takes one keypress and not three. That refinement belongs here when it lands
//! and changes nothing above; the change representation stays char-indexed either way,
//! which is exactly why the ADR separated the two.

use ropey::Rope;

/// The line `pos` sits on.
pub fn line_of(text: &Rope, pos: usize) -> usize {
    text.char_to_line(pos.min(text.len_chars()))
}

/// The column of `pos` within its line, in chars.
pub fn column_of(text: &Rope, pos: usize) -> usize {
    let pos = pos.min(text.len_chars());
    pos - text.line_to_char(text.char_to_line(pos))
}

/// First char of the line `pos` is on.
pub fn line_start(text: &Rope, pos: usize) -> usize {
    text.line_to_char(line_of(text, pos))
}

/// Last char of the line `pos` is on, *before* its terminator — pressing End should land
/// at the end of the visible text, not on the far side of the newline.
pub fn line_end(text: &Rope, pos: usize) -> usize {
    let line = line_of(text, pos);
    let start = text.line_to_char(line);
    let slice = text.line(line);
    let len = slice.len_chars();
    let visible = if len > 0 && slice.char(len - 1) == '\n' { len - 1 } else { len };
    start + visible
}

pub fn left(text: &Rope, pos: usize) -> usize {
    let _ = text;
    pos.saturating_sub(1)
}

pub fn right(text: &Rope, pos: usize) -> usize {
    (pos + 1).min(text.len_chars())
}

/// Move one line up, aiming for `goal` if the caller is tracking a sticky column.
///
/// Returns `pos` unchanged on the first line, so holding Up parks the cursor rather than
/// wrapping around to the end of the file.
pub fn up(text: &Rope, pos: usize, goal: Option<usize>) -> usize {
    let line = line_of(text, pos);
    if line == 0 {
        return pos;
    }
    to_line(text, line - 1, goal.unwrap_or_else(|| column_of(text, pos)))
}

pub fn down(text: &Rope, pos: usize, goal: Option<usize>) -> usize {
    let line = line_of(text, pos);
    if line + 1 >= text.len_lines() {
        return pos;
    }
    to_line(text, line + 1, goal.unwrap_or_else(|| column_of(text, pos)))
}

/// Land on `line` at `column`, or at its end if the line is shorter.
///
/// The column is *not* clamped permanently — that is the caller's sticky-column job. A
/// cursor that steps down past a short line and back up belongs where it started, which
/// only works if the goal outlives the clamp.
fn to_line(text: &Rope, line: usize, column: usize) -> usize {
    let start = text.line_to_char(line);
    let end = line_end(text, start);
    (start + column).min(end)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rope(s: &str) -> Rope {
        Rope::from_str(s)
    }

    #[test]
    fn columns_and_lines_are_reported_from_the_line_start() {
        let t = rope("abc\ndefgh\n");
        assert_eq!((line_of(&t, 0), column_of(&t, 0)), (0, 0));
        assert_eq!((line_of(&t, 2), column_of(&t, 2)), (0, 2));
        assert_eq!((line_of(&t, 4), column_of(&t, 4)), (1, 0));
        assert_eq!((line_of(&t, 7), column_of(&t, 7)), (1, 3));
    }

    #[test]
    fn horizontal_motion_stops_at_both_ends_of_the_document() {
        let t = rope("ab");
        assert_eq!(left(&t, 0), 0, "no wrapping off the front");
        assert_eq!(right(&t, 2), 2, "no running off the end");
        assert_eq!(right(&t, 0), 1);
        assert_eq!(left(&t, 2), 1);
    }

    #[test]
    fn end_lands_before_the_newline_not_after_it() {
        let t = rope("abc\ndef\n");
        assert_eq!(line_end(&t, 0), 3, "the end of 'abc', not the start of 'def'");
        assert_eq!(line_start(&t, 5), 4);
    }

    #[test]
    fn end_of_a_final_line_without_a_trailing_newline() {
        let t = rope("abc\ndef");
        assert_eq!(line_end(&t, 5), 7);
    }

    #[test]
    fn end_of_an_empty_line() {
        let t = rope("abc\n\ndef");
        assert_eq!(line_end(&t, 4), 4, "an empty line starts and ends in the same place");
    }

    #[test]
    fn vertical_motion_parks_at_the_first_and_last_line() {
        let t = rope("abc\ndef");
        assert_eq!(up(&t, 1, None), 1, "already on the first line");
        assert_eq!(down(&t, 5, None), 5, "already on the last line");
    }

    #[test]
    fn moving_down_keeps_the_column() {
        let t = rope("abcdef\nghijkl\n");
        assert_eq!(down(&t, 3, None), 10, "column 3 on the next line");
    }

    #[test]
    fn a_short_line_clamps_the_column_without_losing_it() {
        // The sticky-column rule: stepping through a short line and back must return the
        // cursor to where it started, which only works if the goal survives the clamp.
        let t = rope("abcdefgh\nxy\nabcdefgh\n");
        let start = 6; // line 0, column 6

        let goal = Some(column_of(&t, start));
        let middle = down(&t, start, goal);
        assert_eq!(column_of(&t, middle), 2, "clamped to the short line");

        let back = down(&t, middle, goal);
        assert_eq!(column_of(&t, back), 6, "and restored on the next long one");
    }

    #[test]
    fn without_a_goal_the_column_is_taken_from_where_we_are() {
        let t = rope("abcdefgh\nxy\nabcdefgh\n");
        let middle = down(&t, 6, None);
        let back = down(&t, middle, None);
        assert_eq!(column_of(&t, back), 2, "no sticky column, so the clamp sticks");
    }

    #[test]
    fn motion_is_by_char_across_multibyte_text() {
        let t = rope("héllo");
        assert_eq!(right(&t, 1), 2, "one press crosses 'é' once, not twice");
        assert_eq!(column_of(&t, 5), 5);
    }

    #[test]
    fn positions_past_the_end_are_treated_as_the_end() {
        let t = rope("abc");
        assert_eq!(line_of(&t, 99), 0);
        assert_eq!(column_of(&t, 99), 3);
    }
}
