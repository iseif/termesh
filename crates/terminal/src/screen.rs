//! Stable terminal-screen wrappers over `alacritty_terminal` (ADR-0008 §1).

use std::sync::{Arc, Mutex};

use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config, Osc52, Term, TermMode};
use alacritty_terminal::vte::ansi::{Color, CursorShape, NamedColor, Processor};
use termesh_core::TerminalSize;

use crate::InputModes;

const SCROLLBACK_LINES: usize = 10_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScreenColor {
    Default,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScreenAttributes {
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub inverse: bool,
    pub hidden: bool,
    pub strikeout: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenCell {
    pub symbol: String,
    pub fg: ScreenColor,
    pub bg: ScreenColor,
    pub attributes: ScreenAttributes,
    pub selected: bool,
}

impl Default for ScreenCell {
    fn default() -> Self {
        Self {
            symbol: " ".into(),
            fg: ScreenColor::Default,
            bg: ScreenColor::Default,
            attributes: ScreenAttributes::default(),
            selected: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScreenCursor {
    pub row: usize,
    pub col: usize,
    pub visible: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenSnapshot {
    rows: usize,
    cols: usize,
    cells: Vec<ScreenCell>,
    pub cursor: Option<ScreenCursor>,
}

impl ScreenSnapshot {
    pub fn rows(&self) -> usize {
        self.rows
    }

    pub fn cols(&self) -> usize {
        self.cols
    }

    pub fn cell(&self, row: usize, col: usize) -> &ScreenCell {
        &self.cells[row * self.cols + col]
    }

    pub fn text_at(&self, row: usize, col: usize, len: usize) -> String {
        let end = (col + len).min(self.cols);
        (col..end).map(|column| self.cell(row, column).symbol.as_str()).collect()
    }

    pub fn plain_text(&self) -> String {
        let mut text = String::new();
        for row in 0..self.rows {
            if row > 0 {
                text.push('\n');
            }
            for col in 0..self.cols {
                text.push_str(&self.cell(row, col).symbol);
            }
        }
        text
    }
}

#[derive(Debug, Clone, Copy)]
struct ScreenDimensions {
    rows: usize,
    cols: usize,
}

impl From<TerminalSize> for ScreenDimensions {
    fn from(size: TerminalSize) -> Self {
        Self { rows: usize::from(size.rows.max(1)), cols: usize::from(size.cols.max(1)) }
    }
}

impl Dimensions for ScreenDimensions {
    fn total_lines(&self) -> usize {
        self.rows
    }

    fn screen_lines(&self) -> usize {
        self.rows
    }

    fn columns(&self) -> usize {
        self.cols
    }
}

#[derive(Debug)]
struct EventState {
    responses: Vec<Vec<u8>>,
    title: Option<String>,
    size: TerminalSize,
}

#[derive(Clone)]
struct EventProxy {
    state: Arc<Mutex<EventState>>,
}

impl EventListener for EventProxy {
    fn send_event(&self, event: Event) {
        let mut state = self.state.lock().expect("terminal event state poisoned");
        match event {
            Event::PtyWrite(text) => state.responses.push(text.into_bytes()),
            Event::TextAreaSizeRequest(formatter) => {
                let size = state.size;
                let response = formatter(WindowSize {
                    num_lines: size.rows,
                    num_cols: size.cols,
                    cell_width: 0,
                    cell_height: 0,
                });
                state.responses.push(response.into_bytes());
            }
            Event::Title(title) => state.title = Some(title),
            Event::ResetTitle => state.title = None,
            // Clipboard events are intentionally ignored. Only a human copy action can
            // reach ClipboardService (ADR-0008 §3).
            Event::ClipboardStore(..) | Event::ClipboardLoad(..) => {}
            _ => {}
        }
    }
}

pub struct TerminalScreen {
    term: Term<EventProxy>,
    processor: Processor,
    events: Arc<Mutex<EventState>>,
    selection_cursor: Option<Point>,
    /// Where the selection was begun. Kept alongside alacritty's own state because the
    /// *sides* of both ends have to be recomputed whenever the direction of travel
    /// flips, and `Selection` does not expose its anchor.
    selection_anchor: Option<Point>,
}

impl TerminalScreen {
    pub fn new(size: TerminalSize) -> Self {
        let clamped = TerminalSize { rows: size.rows.max(1), cols: size.cols.max(1) };
        let events =
            Arc::new(Mutex::new(EventState { responses: Vec::new(), title: None, size: clamped }));
        let proxy = EventProxy { state: events.clone() };
        let config = Config {
            scrolling_history: SCROLLBACK_LINES,
            osc52: Osc52::Disabled,
            ..Config::default()
        };
        let dimensions = ScreenDimensions::from(clamped);

        Self {
            term: Term::new(config, &dimensions, proxy),
            processor: Processor::new(),
            events,
            selection_cursor: None,
            selection_anchor: None,
        }
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        self.processor.advance(&mut self.term, bytes);
    }

    pub fn resize(&mut self, size: TerminalSize) {
        let clamped = TerminalSize { rows: size.rows.max(1), cols: size.cols.max(1) };
        self.events.lock().expect("terminal event state poisoned").size = clamped;
        self.term.resize(ScreenDimensions::from(clamped));
    }

    pub fn scroll_lines(&mut self, delta: i32) {
        self.term.scroll_display(Scroll::Delta(delta));
    }

    pub fn snapshot(&self) -> ScreenSnapshot {
        let rows = self.term.screen_lines();
        let cols = self.term.columns();
        let mut cells = vec![ScreenCell::default(); rows * cols];
        let content = self.term.renderable_content();
        let display_offset = content.display_offset;
        let selection = content.selection;
        let cursor_point = content.cursor.point;
        let cursor_shape = content.cursor.shape;

        for indexed in content.display_iter {
            let row = indexed.point.line.0 + display_offset as i32;
            if row < 0 || row >= rows as i32 {
                continue;
            }
            let col = indexed.point.column.0;
            let flags = indexed.cell.flags;
            let attributes = ScreenAttributes {
                bold: flags.contains(Flags::BOLD),
                dim: flags.contains(Flags::DIM),
                italic: flags.contains(Flags::ITALIC),
                underline: flags.intersects(Flags::ALL_UNDERLINES),
                inverse: flags.contains(Flags::INVERSE),
                hidden: flags.contains(Flags::HIDDEN),
                strikeout: flags.contains(Flags::STRIKEOUT),
            };
            let spacer =
                flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER);
            let mut symbol = if spacer || attributes.hidden {
                String::new()
            } else {
                // A grid cell is not always a printable character. Alacritty stores the
                // literal '\t' in every cell a tab stop covers, so that copying a region
                // gives back real tabs instead of runs of spaces — see `Term::put_tab`.
                // That is right for the clipboard and wrong for a renderer: this snapshot
                // is what gets drawn, and a control character there panics ratatui in
                // debug and draws garbage in release. Blank it. Copy still goes through
                // `selected_text`, straight off the grid, so real tabs survive there.
                printable(indexed.cell.c).unwrap_or(' ').to_string()
            };
            if !spacer && !attributes.hidden {
                if let Some(zerowidth) = indexed.cell.zerowidth() {
                    symbol.extend(zerowidth.iter().copied().filter_map(printable));
                }
            }

            cells[row as usize * cols + col] = ScreenCell {
                symbol,
                fg: map_color(indexed.cell.fg),
                bg: map_color(indexed.cell.bg),
                attributes,
                selected: selection.is_some_and(|range| range.contains(indexed.point)),
            };
        }

        let cursor_row = cursor_point.line.0 + display_offset as i32;
        let cursor = (0..rows as i32).contains(&cursor_row).then_some(ScreenCursor {
            row: cursor_row.max(0) as usize,
            col: cursor_point.column.0,
            visible: cursor_shape != CursorShape::Hidden,
        });

        ScreenSnapshot { rows, cols, cells, cursor }
    }

    pub fn take_responses(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.events.lock().expect("terminal event state poisoned").responses)
    }

    pub fn begin_selection(&mut self) {
        self.term.scroll_display(Scroll::Bottom);
        let point = self.term.grid().cursor.point;
        self.selection_cursor = Some(point);
        self.selection_anchor = Some(point);
        self.term.selection = Some(Selection::new(SelectionType::Simple, point, Side::Left));
    }

    pub fn move_selection(&mut self, row_delta: i32, col_delta: i32, extend: bool) {
        let Some(current) = self.selection_cursor else {
            return;
        };
        let cols = self.term.columns() as i64;
        let top = self.term.topmost_line().0 as i64;
        let bottom = self.term.bottommost_line().0 as i64;
        let current_index = (i64::from(current.line.0) - top) * cols + current.column.0 as i64;
        let max_index = (bottom - top + 1) * cols - 1;
        let delta = i64::from(row_delta) * cols + i64::from(col_delta);
        let next_index = (current_index + delta).clamp(0, max_index);
        let point = Point::new(
            Line((top + next_index / cols) as i32),
            Column((next_index % cols) as usize),
        );
        self.selection_cursor = Some(point);

        if extend {
            // Both ends are rebuilt from the anchor rather than only moving the far end.
            // alacritty trims a cell whenever a range *starts* on a right edge or *ends*
            // on a left edge, so the sides have to mirror the direction of travel or the
            // selection loses a cell at each end the moment it runs backwards.
            let anchor = self.selection_anchor.unwrap_or(point);
            let (anchor_side, point_side) =
                if point < anchor { (Side::Right, Side::Left) } else { (Side::Left, Side::Right) };
            let mut selection = Selection::new(SelectionType::Simple, anchor, anchor_side);
            selection.update(point, point_side);
            self.term.selection = Some(selection);
        } else {
            self.selection_anchor = Some(point);
            self.term.selection = Some(Selection::new(SelectionType::Simple, point, Side::Left));
        }

        let viewport_top = -(self.term.grid().display_offset() as i32);
        let viewport_bottom = viewport_top + self.term.screen_lines() as i32 - 1;
        if point.line.0 < viewport_top {
            self.term.scroll_display(Scroll::Delta(viewport_top - point.line.0));
        } else if point.line.0 > viewport_bottom {
            self.term.scroll_display(Scroll::Delta(viewport_bottom - point.line.0));
        }
    }

    pub fn selected_text(&self) -> Option<String> {
        self.term.selection_to_string()
    }

    pub fn clear_selection(&mut self) {
        self.term.selection = None;
        self.selection_cursor = None;
        self.selection_anchor = None;
    }

    pub fn history_size(&self) -> usize {
        self.term.history_size()
    }

    pub fn input_modes(&self) -> InputModes {
        InputModes { application_cursor: self.term.mode().contains(TermMode::APP_CURSOR) }
    }

    pub fn title(&self) -> Option<String> {
        self.events.lock().expect("terminal event state poisoned").title.clone()
    }
}

/// The character to draw for a grid cell, if it can be drawn at all.
///
/// Control characters cannot: they have no width, so a renderer handed one either panics
/// or misaligns every column after it. They reach the grid because alacritty keeps some
/// deliberately (tab stops), so filter at the point of drawing rather than trusting the
/// model to hold only printable text.
fn printable(c: char) -> Option<char> {
    (!c.is_control()).then_some(c)
}

fn map_color(color: Color) -> ScreenColor {
    match color {
        Color::Spec(rgb) => ScreenColor::Rgb(rgb.r, rgb.g, rgb.b),
        Color::Indexed(index) => ScreenColor::Indexed(index),
        Color::Named(named) => match named {
            NamedColor::Black | NamedColor::DimBlack => ScreenColor::Indexed(0),
            NamedColor::Red | NamedColor::DimRed => ScreenColor::Indexed(1),
            NamedColor::Green | NamedColor::DimGreen => ScreenColor::Indexed(2),
            NamedColor::Yellow | NamedColor::DimYellow => ScreenColor::Indexed(3),
            NamedColor::Blue | NamedColor::DimBlue => ScreenColor::Indexed(4),
            NamedColor::Magenta | NamedColor::DimMagenta => ScreenColor::Indexed(5),
            NamedColor::Cyan | NamedColor::DimCyan => ScreenColor::Indexed(6),
            NamedColor::White | NamedColor::DimWhite => ScreenColor::Indexed(7),
            NamedColor::BrightBlack => ScreenColor::Indexed(8),
            NamedColor::BrightRed => ScreenColor::Indexed(9),
            NamedColor::BrightGreen => ScreenColor::Indexed(10),
            NamedColor::BrightYellow => ScreenColor::Indexed(11),
            NamedColor::BrightBlue => ScreenColor::Indexed(12),
            NamedColor::BrightMagenta => ScreenColor::Indexed(13),
            NamedColor::BrightCyan => ScreenColor::Indexed(14),
            NamedColor::BrightWhite => ScreenColor::Indexed(15),
            NamedColor::Foreground
            | NamedColor::Background
            | NamedColor::Cursor
            | NamedColor::BrightForeground
            | NamedColor::DimForeground => ScreenColor::Default,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use termesh_core::TerminalSize;

    #[test]
    fn ansi_color_and_cursor_movement_change_the_snapshot() {
        let mut screen = TerminalScreen::new(TerminalSize { rows: 3, cols: 8 });
        screen.feed(b"one\r\n\x1b[31mtwo\x1b[0m");
        let snap = screen.snapshot();
        assert_eq!(snap.text_at(0, 0, 3), "one");
        assert_eq!(snap.text_at(1, 0, 3), "two");
        assert_eq!(snap.cell(1, 0).fg, ScreenColor::Indexed(1));
    }

    #[test]
    fn scrollback_is_bounded() {
        let mut screen = TerminalScreen::new(TerminalSize { rows: 2, cols: 8 });
        for i in 0..10_050 {
            screen.feed(format!("{i}\r\n").as_bytes());
        }
        assert!(screen.history_size() <= 10_000);
    }

    #[test]
    fn child_clipboard_requests_are_not_forwarded() {
        let mut screen = TerminalScreen::new(TerminalSize { rows: 2, cols: 8 });
        screen.feed(b"\x1b]52;c;dGVzdA==\x07");
        assert!(screen.take_responses().is_empty());
    }

    #[test]
    fn terminal_queries_queue_bytes_for_the_pty() {
        let mut screen = TerminalScreen::new(TerminalSize { rows: 2, cols: 8 });
        screen.feed(b"\x1b[6n");
        assert_eq!(screen.take_responses(), [b"\x1b[1;1R".to_vec()]);
        assert!(screen.take_responses().is_empty(), "responses are drained exactly once");
    }

    #[test]
    fn application_cursor_mode_is_exposed_to_the_input_encoder() {
        let mut screen = TerminalScreen::new(TerminalSize { rows: 2, cols: 8 });
        assert!(!screen.input_modes().application_cursor);
        screen.feed(b"\x1b[?1h");
        assert!(screen.input_modes().application_cursor);
    }

    #[test]
    fn recorded_streams_preserve_terminal_semantics() {
        let mut screen = TerminalScreen::new(TerminalSize { rows: 3, cols: 12 });
        screen.feed(b"stale\x1b[2J\x1b[Hnew\r\n\x1b[38;5;123mI\x1b[38;2;1;2;3mR");
        let snap = screen.snapshot();
        assert_eq!(snap.text_at(0, 0, 3), "new");
        assert_eq!(snap.cell(1, 0).fg, ScreenColor::Indexed(123));
        assert_eq!(snap.cell(1, 1).fg, ScreenColor::Rgb(1, 2, 3));
    }

    #[test]
    fn attributes_are_exposed_without_alacritty_types() {
        let mut screen = TerminalScreen::new(TerminalSize { rows: 2, cols: 8 });
        screen.feed(b"\x1b[1;2;3;4;7mX");
        let attrs = screen.snapshot().cell(0, 0).attributes;
        assert!(attrs.bold);
        assert!(attrs.dim);
        assert!(attrs.italic);
        assert!(attrs.underline);
        assert!(attrs.inverse);
    }

    #[test]
    fn alternate_screen_restores_primary_contents() {
        let mut screen = TerminalScreen::new(TerminalSize { rows: 2, cols: 8 });
        screen.feed(b"main\x1b[?1049halt\x1b[?1049l");
        assert_eq!(screen.snapshot().text_at(0, 0, 4), "main");
    }

    #[test]
    fn carriage_return_and_backspace_rewrite_existing_cells() {
        let mut screen = TerminalScreen::new(TerminalSize { rows: 2, cols: 8 });
        screen.feed(b"abc\rZ\x08Y");
        assert_eq!(screen.snapshot().text_at(0, 0, 3), "Ybc");
    }

    #[test]
    fn title_changes_are_domain_metadata() {
        let mut screen = TerminalScreen::new(TerminalSize { rows: 2, cols: 8 });
        screen.feed(b"\x1b]2;cargo test\x07");
        assert_eq!(screen.title().as_deref(), Some("cargo test"));
    }

    #[test]
    fn wide_and_combining_characters_survive_snapshot_and_resize() {
        let mut screen = TerminalScreen::new(TerminalSize { rows: 2, cols: 8 });
        screen.feed("界e\u{301}".as_bytes());
        assert_eq!(screen.snapshot().plain_text().trim_end(), "界e\u{301}");
        screen.resize(TerminalSize { rows: 3, cols: 12 });
        let snap = screen.snapshot();
        assert_eq!(snap.rows(), 3);
        assert_eq!(snap.cols(), 12);
        assert_eq!(snap.plain_text().lines().next().unwrap().trim_end(), "界e\u{301}");
    }

    #[test]
    fn selection_text_comes_from_the_terminal_grid() {
        let mut screen = TerminalScreen::new(TerminalSize { rows: 2, cols: 8 });
        screen.feed(b"hello");
        screen.begin_selection();
        screen.move_selection(0, -5, false);
        screen.move_selection(0, 4, true);
        assert_eq!(screen.selected_text().as_deref(), Some("hello"));
    }

    /// Regression: `move_selection` always anchored to `Side::Right`, so extending
    /// leftward shrank *both* ends — `hello` came back as `ello` and column 0 could
    /// never be reached. Only the rightward case was covered, which is why it survived.
    #[test]
    fn extending_a_selection_leftward_keeps_both_end_cells() {
        let mut screen = TerminalScreen::new(TerminalSize { rows: 2, cols: 8 });
        screen.feed(b"hello");
        screen.begin_selection();
        // The cursor sits past 'o'; step back onto it, then extend left over the word.
        screen.move_selection(0, -1, false);
        for _ in 0..4 {
            screen.move_selection(0, -1, true);
        }
        assert_eq!(screen.selected_text().as_deref(), Some("hello"));
    }

    /// Regression: alacritty stores the literal `'\t'` in every cell a tab stop covers,
    /// so that copying a region yields real tabs rather than runs of spaces. It is still
    /// a control character, and handing one to a renderer panics. `git status` indents
    /// its file list with tabs, which crashed the app the moment the output landed.
    #[test]
    fn a_tab_stop_does_not_leak_a_control_character_into_the_snapshot() {
        let mut screen = TerminalScreen::new(TerminalSize { rows: 2, cols: 16 });
        screen.feed(b"a\tb\r\n\tmodified: x");

        let snapshot = screen.snapshot();
        let offenders: Vec<&str> = snapshot
            .cells
            .iter()
            .map(|cell| cell.symbol.as_str())
            .filter(|symbol| symbol.chars().any(char::is_control))
            .collect();

        assert!(offenders.is_empty(), "control characters reached the snapshot: {offenders:?}");
    }

    /// The tab still has to *look* like a tab — cells it covers render blank, not as a
    /// visible glyph, and the text after it stays at the tab stop.
    #[test]
    fn a_tab_still_advances_to_its_stop() {
        let mut screen = TerminalScreen::new(TerminalSize { rows: 1, cols: 16 });
        screen.feed(b"a\tb");
        let snapshot = screen.snapshot();
        let row: String = snapshot.cells.iter().map(|cell| cell.symbol.as_str()).collect();
        assert_eq!(row.trim_end(), "a       b", "tab should reach the 8-column stop");
    }

    #[test]
    fn clearing_selection_removes_copy_state() {
        let mut screen = TerminalScreen::new(TerminalSize { rows: 2, cols: 8 });
        screen.feed(b"hello");
        screen.begin_selection();
        screen.clear_selection();
        assert_eq!(screen.selected_text(), None);
    }
}
