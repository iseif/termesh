//! Char offsets → screen cells.
//!
//! The editor thinks in char offsets (ADR-0006 §1); a terminal thinks in cells, and the
//! two disagree whenever a line contains a tab or a wide character. Getting this wrong
//! puts the cursor three cells left of the text it is on for every preceding tab.
//!
//! It lives here, once, because everything that draws into the text area needs the same
//! conversion — the cursor now, agent diff hunks next. Two implementations of this would
//! disagree, and the disagreement would look like a rendering bug rather than an
//! arithmetic one.

use unicode_width::UnicodeWidthChar;

/// Cells between tab stops, when nothing more specific is known. `config.toml`'s
/// `tab_width` (ADR-0014 Task 3) overrides this at every call site that renders a buffer;
/// this constant remains the value for call sites that provably never see a tab (a char
/// already expanded by [`expand_tabs`]) and for tests that do not care.
pub const TAB_WIDTH: usize = 4;

/// The width of one char at `column`, in cells.
///
/// Tabs advance to the next tab stop, so their width depends on where they start. Control
/// characters render as nothing.
fn char_width(c: char, column: usize, tab_width: usize) -> usize {
    match c {
        '\t' => tab_width - (column % tab_width),
        _ => c.width().unwrap_or(0),
    }
}

/// Render `line` for display: tabs expanded to their stops, trailing newline dropped.
pub fn expand_tabs(line: &str, tab_width: usize) -> String {
    let mut out = String::with_capacity(line.len());
    let mut column = 0;
    for c in line.trim_end_matches('\n').chars() {
        if c == '\t' {
            let spaces = char_width('\t', column, tab_width);
            out.extend(std::iter::repeat_n(' ', spaces));
            column += spaces;
        } else {
            out.push(c);
            column += char_width(c, column, tab_width);
        }
    }
    out
}

/// The screen column a char offset lands on within `line`.
///
/// Offsets past the end of the line clamp to its display width, so a cursor at the end of
/// a line lands just past its last character rather than off in space.
pub fn display_column(line: &str, char_offset: usize, tab_width: usize) -> usize {
    let mut column = 0;
    for c in line.trim_end_matches('\n').chars().take(char_offset) {
        column += char_width(c, column, tab_width);
    }
    column
}

/// The display width of a whole line.
pub fn display_width(line: &str, tab_width: usize) -> usize {
    display_column(line, usize::MAX, tab_width)
}

/// Wrap `text` to `width` cells, breaking on spaces and honouring explicit newlines.
///
/// Ratatui's own wrapping breaks mid-word and gives no way to count the resulting lines,
/// which is what scrolling needs. Doing it here means the caller knows exactly how tall
/// the content is — and `~/.config/termesh/agents.toml` stops being split into `agent` and
/// `s.toml`.
///
/// Measured in display cells, so a line of CJK wraps at half as many characters.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return Vec::new();
    }
    let mut out = Vec::new();

    for paragraph in text.split('\n') {
        let mut line = String::new();
        let mut used = 0;

        for word in paragraph.split(' ') {
            // Terminal and agent text, never a buffer line: no tab expansion applies here.
            let w = display_width(word, TAB_WIDTH);

            // A word longer than the pane has to break somewhere; break it at the edge
            // rather than letting it push everything else off screen.
            if w > width {
                if !line.is_empty() {
                    out.push(std::mem::take(&mut line));
                    used = 0;
                }
                for ch in word.chars() {
                    let cw = display_width(&ch.to_string(), TAB_WIDTH);
                    if used + cw > width {
                        out.push(std::mem::take(&mut line));
                        used = 0;
                    }
                    line.push(ch);
                    used += cw;
                }
                continue;
            }

            let needed = if line.is_empty() { w } else { w + 1 };
            if used + needed > width {
                out.push(std::mem::take(&mut line));
                used = 0;
            }
            if !line.is_empty() {
                line.push(' ');
                used += 1;
            }
            line.push_str(word);
            used += w;
        }
        out.push(line);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_ascii_maps_one_to_one() {
        assert_eq!(display_column("hello", 3, TAB_WIDTH), 3);
        assert_eq!(expand_tabs("hello", TAB_WIDTH), "hello");
    }

    #[test]
    fn a_tab_advances_to_the_next_stop_not_a_fixed_width() {
        // The bug a blind four-space replacement hides: a tab after one character
        // advances three cells, not four.
        assert_eq!(expand_tabs("\tx", TAB_WIDTH), "    x");
        assert_eq!(expand_tabs("a\tx", TAB_WIDTH), "a   x");
        assert_eq!(expand_tabs("abc\tx", TAB_WIDTH), "abc x");
        assert_eq!(expand_tabs("abcd\tx", TAB_WIDTH), "abcd    x");
    }

    #[test]
    fn a_configured_tab_width_changes_the_stop() {
        assert_eq!(expand_tabs("\tx", 2), "  x");
        assert_eq!(expand_tabs("a\tx", 8), "a       x");
    }

    #[test]
    fn the_cursor_column_agrees_with_the_expanded_line() {
        for line in ["\tx", "a\tx", "abc\tx", "abcd\tx", "\t\tx"] {
            let offset = line.chars().count() - 1; // the 'x'
            let expanded = expand_tabs(line, TAB_WIDTH);
            assert_eq!(
                display_column(line, offset, TAB_WIDTH),
                expanded.find('x').unwrap(),
                "cursor and text disagree on {line:?}"
            );
        }
    }

    #[test]
    fn wide_characters_take_two_cells() {
        // CJK and most emoji are double-width; a char offset is not a screen column.
        assert_eq!(display_column("世界x", 2, TAB_WIDTH), 4);
        assert_eq!(display_width("世界", TAB_WIDTH), 4);
    }

    #[test]
    fn combining_marks_take_none() {
        // "e" + combining acute is two chars but one cell.
        assert_eq!(display_width("e\u{0301}", TAB_WIDTH), 1);
    }

    #[test]
    fn an_offset_past_the_end_clamps_to_the_line_width() {
        assert_eq!(display_column("abc", 99, TAB_WIDTH), 3);
        assert_eq!(display_column("", 5, TAB_WIDTH), 0);
    }

    #[test]
    fn the_trailing_newline_is_not_part_of_the_line() {
        assert_eq!(expand_tabs("abc\n", TAB_WIDTH), "abc");
        assert_eq!(display_width("abc\n", TAB_WIDTH), 3);
    }

    // --- wrapping ------------------------------------------------------------------

    #[test]
    fn text_wraps_on_spaces_not_mid_word() {
        assert_eq!(wrap("the quick brown fox", 10), ["the quick", "brown fox"]);
    }

    #[test]
    fn explicit_newlines_are_kept() {
        assert_eq!(wrap("one\n\ntwo", 20), ["one", "", "two"]);
    }

    #[test]
    fn a_word_longer_than_the_pane_breaks_at_the_edge() {
        // Otherwise one long path pushes everything else off screen.
        assert_eq!(
            wrap("~/.config/termesh/agents.toml", 12),
            ["~/.config/te", "rmesh/agents", ".toml"]
        );
    }

    #[test]
    fn a_long_word_does_not_swallow_the_line_before_it() {
        assert_eq!(wrap("see aaaaaaaaaaaaaa", 8), ["see", "aaaaaaaa", "aaaaaa"]);
    }

    #[test]
    fn wrapping_measures_cells_so_wide_glyphs_take_two() {
        assert_eq!(wrap("世界世界世", 6), ["世界世", "界世"]);
    }

    #[test]
    fn a_zero_width_pane_wraps_to_nothing_rather_than_looping() {
        assert!(wrap("anything", 0).is_empty());
    }

    #[test]
    fn every_wrapped_line_fits() {
        let text = "The user is asking what this module does. I already have its source.";
        for width in [8, 12, 24, 40] {
            for line in wrap(text, width) {
                assert!(display_width(&line, TAB_WIDTH) <= width, "{line:?} exceeds {width}");
            }
        }
    }

    #[test]
    fn tab_stops_survive_wide_characters_before_them() {
        // The tab measures from the *display* column, so a preceding wide char shifts it.
        assert_eq!(expand_tabs("世\tx", TAB_WIDTH), "世  x");
    }
}
