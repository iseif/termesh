//! Stateless render helpers. They take primitive data (not app types), so the `ui`
//! crate never depends on `app` — composition happens in `app::view` (ARCHITECTURE.md §7).
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};
use ratatui::Frame;

use crate::theme::Theme;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalColor {
    Default,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TerminalAttributes {
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub inverse: bool,
    pub hidden: bool,
    pub strikeout: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalCell {
    pub row: usize,
    pub col: usize,
    pub symbol: String,
    pub fg: TerminalColor,
    pub bg: TerminalColor,
    pub attributes: TerminalAttributes,
    pub selected: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalCursor {
    pub row: usize,
    pub col: usize,
    pub visible: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalTab {
    pub label: String,
    pub status: String,
    pub active: bool,
}

/// Render a terminal grid and its retained-session tab strip from backend-neutral data.
#[allow(clippy::too_many_arguments)]
pub fn terminal(
    frame: &mut Frame,
    area: Rect,
    focused: bool,
    tabs: &[TerminalTab],
    cells: &[TerminalCell],
    cursor: Option<TerminalCursor>,
    copy_mode: bool,
    theme: &Theme,
) {
    let marker = if focused { " \u{25CF} " } else { " " };
    let mode = if copy_mode { " \u{00B7} COPY" } else { "" };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.border_style(focused))
        .title(Span::styled(format!("{marker}Terminal{mode} "), theme.title_style(focused)));
    let mut inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    if !tabs.is_empty() {
        let strip = Rect { height: 1, ..inner };
        frame.render_widget(Paragraph::new(terminal_tab_strip(tabs, theme)), strip);
        inner = Rect { y: inner.y + 1, height: inner.height.saturating_sub(1), ..inner };
    }
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    {
        let buffer = frame.buffer_mut();
        for source in cells {
            let Ok(col) = u16::try_from(source.col) else {
                continue;
            };
            let Ok(row) = u16::try_from(source.row) else {
                continue;
            };
            if col >= inner.width || row >= inner.height {
                continue;
            }
            let Some(cell) = buffer.cell_mut((inner.x + col, inner.y + row)) else {
                continue;
            };
            // `set_symbol` is a low-level API that trusts its caller: ratatui filters
            // control characters in `Span`/`set_stringn`, not here, and one that slips
            // through panics the buffer in debug and misaligns the row in release. The
            // screen model already blanks them, so this is the belt to that braces — the
            // cost of being wrong is the whole app going down mid-keystroke.
            let renderable = source.symbol.trim_matches(char::is_control);
            cell.set_symbol(if renderable.is_empty() { " " } else { renderable });

            let mut modifiers = Modifier::empty();
            if source.attributes.bold {
                modifiers |= Modifier::BOLD;
            }
            if source.attributes.dim {
                modifiers |= Modifier::DIM;
            }
            if source.attributes.italic {
                modifiers |= Modifier::ITALIC;
            }
            if source.attributes.underline {
                modifiers |= Modifier::UNDERLINED;
            }
            if source.attributes.inverse {
                modifiers |= Modifier::REVERSED;
            }
            if source.attributes.hidden {
                modifiers |= Modifier::HIDDEN;
            }
            if source.attributes.strikeout {
                modifiers |= Modifier::CROSSED_OUT;
            }
            let mut style = Style::default()
                .fg(theme.terminal_color(source.fg))
                .bg(theme.terminal_color(source.bg))
                .add_modifier(modifiers);
            if source.selected {
                style = style.fg(theme.sel_fg).bg(theme.sel_bg);
            }
            cell.set_style(style);
        }
    }

    if focused {
        if let Some(cursor) = cursor.filter(|cursor| cursor.visible) {
            if let (Ok(col), Ok(row)) = (u16::try_from(cursor.col), u16::try_from(cursor.row)) {
                if col < inner.width && row < inner.height {
                    frame.set_cursor_position((inner.x + col, inner.y + row));
                }
            }
        }
    }
}

fn terminal_tab_strip(tabs: &[TerminalTab], theme: &Theme) -> Line<'static> {
    let mut spans = Vec::new();
    for (index, tab) in tabs.iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled("\u{2502}", Style::default().fg(theme.border)));
        }
        let label = format!(" {}  {} ", tab.label, tab.status);
        let style =
            if tab.active { theme.selection_style() } else { Style::default().fg(theme.dim) };
        spans.push(Span::styled(label, style));
    }
    Line::from(spans)
}

/// A bordered pane with a focus-aware title, filled with wrapped body text.
pub fn pane(frame: &mut Frame, area: Rect, title: &str, focused: bool, body: &str, theme: &Theme) {
    pane_scrolled(frame, area, title, focused, body, 0, theme);
}

/// A bordered pane whose body can be scrolled back through.
///
/// `scrollback` counts wrapped lines *up from the bottom*, so new content appearing does
/// not shift what the reader is looking at, and 0 always means "the latest".
///
/// Returns how many lines the body actually has, so the caller can clamp its own scroll
/// state rather than guessing at the pane's geometry.
pub fn pane_scrolled(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    focused: bool,
    body: &str,
    scrollback: usize,
    theme: &Theme,
) -> usize {
    let marker = if focused { " \u{25CF} " } else { " " };

    // Measure against the body area the block will leave us. Borders cost one cell a side.
    let inner_w = area.width.saturating_sub(2) as usize;
    let inner_h = area.height.saturating_sub(2) as usize;
    if inner_w == 0 || inner_h == 0 {
        frame.render_widget(Block::default().borders(Borders::ALL), area);
        return 0;
    }

    // Wrapped here rather than by ratatui so the line count is known: scrolling needs it,
    // and ratatui's wrapping does not expose it.
    let lines = crate::text::wrap(body, inner_w);
    let max_scroll = lines.len().saturating_sub(inner_h);
    let from_bottom = scrollback.min(max_scroll);
    let top = max_scroll - from_bottom;

    // How much is hidden above, shown in the title rather than over the text. Painting it
    // into the body would cost a line of the very content it is reporting on.
    let hidden = if top > 0 { format!("\u{2191}{top} ") } else { String::new() };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.border_style(focused))
        .title(Span::styled(format!("{marker}{title} {hidden}"), theme.title_style(focused)));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let visible: Vec<Line> = lines
        .iter()
        .skip(top)
        .take(inner_h)
        .map(|l| Line::from(Span::styled(l.clone(), Style::default().fg(theme.fg))))
        .collect();
    frame.render_widget(Paragraph::new(visible), inner);

    lines.len()
}

/// The bottom status bar: hints (or a transient notification) on the left, context on the right.
pub fn status_bar(
    frame: &mut Frame,
    area: Rect,
    hints: &str,
    right: &str,
    notification: Option<&str>,
    theme: &Theme,
) {
    frame.render_widget(Block::default().style(Style::default().bg(theme.statusbar_bg)), area);
    let right_w = right.chars().count() as u16 + 1;
    let parts = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(0), Constraint::Length(right_w)])
        .split(area);

    let left_line = match notification {
        Some(n) => Line::styled(format!(" {n}"), Style::default().fg(theme.accent)),
        None => Line::styled(format!(" {hints}"), Style::default().fg(theme.statusbar_fg)),
    };
    frame.render_widget(
        Paragraph::new(left_line).style(Style::default().bg(theme.statusbar_bg)),
        parts[0],
    );
    frame.render_widget(
        Paragraph::new(Line::styled(right, Style::default().fg(theme.statusbar_dim)))
            .alignment(Alignment::Right)
            .style(Style::default().bg(theme.statusbar_bg)),
        parts[1],
    );
}

/// One rendered line of the file explorer.
///
/// Plain data on purpose: `ui` must not depend on `filesystem`, so `app::view` flattens
/// tree rows into these (ARCHITECTURE.md §7 — widgets render state, they don't own it).
pub struct TreeLine {
    pub depth: usize,
    pub name: String,
    pub is_dir: bool,
    pub expanded: bool,
    pub loading: bool,
    pub error: Option<String>,
    pub marker: Option<char>,
}

/// The file explorer: an indented, scrollable tree with the selection highlighted.
pub fn file_tree(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    focused: bool,
    lines: &[TreeLine],
    selected: usize,
    theme: &Theme,
) {
    let marker = if focused { " \u{25CF} " } else { " " };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.border_style(focused))
        .title(Span::styled(format!("{marker}{title} "), theme.title_style(focused)));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let items: Vec<ListItem> = lines
        .iter()
        .map(|line| {
            // Two spaces per level, with the chevron column reserved on every row so
            // files line up under the directories that contain them.
            let indent = "  ".repeat(line.depth);
            let chevron = if !line.is_dir {
                "  "
            } else if line.expanded {
                "\u{25BE} " // ▾
            } else {
                "\u{25B8} " // ▸
            };

            let mut spans = vec![
                Span::raw(indent),
                Span::styled(chevron, Style::default().fg(theme.dim)),
                Span::styled(
                    line.name.clone(),
                    if line.is_dir {
                        Style::default().fg(theme.accent)
                    } else {
                        Style::default().fg(theme.fg)
                    },
                ),
            ];

            // A directory that failed to open must say so, or it is indistinguishable
            // from an empty one.
            if let Some(err) = &line.error {
                spans.push(Span::styled(format!("  ({err})"), Style::default().fg(theme.dim)));
            } else if line.loading {
                spans.push(Span::styled("  \u{2026}", Style::default().fg(theme.dim)));
            }
            if let Some(marker) = line.marker {
                let color = if marker == '!' { theme.warn } else { theme.accent };
                spans.push(Span::styled(format!("  {marker}"), Style::default().fg(color)));
            }

            ListItem::new(Line::from(spans))
        })
        .collect();

    let mut state = ListState::default();
    if !lines.is_empty() {
        state.select(Some(selected.min(lines.len() - 1)));
    }
    frame.render_stateful_widget(
        List::new(items).highlight_style(theme.selection_style()),
        inner,
        &mut state,
    );
}

/// A single-line prompt overlay (new file, rename, delete confirmation).
pub fn prompt(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    input: &str,
    takes_input: bool,
    theme: &Theme,
) {
    let rect = crate::layout::centered_rect(60, 20, area);
    frame.render_widget(Clear, rect);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border_focus))
        .title(Span::styled(format!(" {title} "), theme.title_style(true)));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0)])
        .split(inner);

    let line = if takes_input {
        Line::from(vec![
            Span::styled("\u{203A} ", Style::default().fg(theme.accent)),
            Span::styled(input, Style::default().fg(theme.fg)),
            Span::styled("\u{2581}", Style::default().fg(theme.dim)),
        ])
    } else {
        Line::styled("Enter to confirm \u{00B7} Esc to cancel", Style::default().fg(theme.dim))
    };
    frame.render_widget(Paragraph::new(line), rows[0]);

    if takes_input {
        frame.render_widget(
            Paragraph::new(Line::styled(
                "Enter to confirm \u{00B7} Esc to cancel",
                Style::default().fg(theme.dim),
            )),
            rows[1],
        );
    }
}

/// The command palette overlay. `items` is `(label, hint)` pairs already filtered.
pub fn command_palette(
    frame: &mut Frame,
    area: Rect,
    query: &str,
    items: &[(String, String)],
    selected: usize,
    total: usize,
    theme: &Theme,
) {
    let rect = crate::layout::centered_rect(60, 60, area);
    frame.render_widget(Clear, rect);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border_focus))
        .title(Span::styled(
            format!(" Actions ({}/{}) ", items.len(), total),
            theme.title_style(true),
        ));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0)])
        .split(inner);

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("\u{203A} ", Style::default().fg(theme.accent)),
            Span::styled(query, Style::default().fg(theme.fg)),
            Span::styled("\u{2581}", Style::default().fg(theme.dim)),
        ])),
        rows[0],
    );

    let list_items: Vec<ListItem> = items
        .iter()
        .map(|(label, hint)| {
            ListItem::new(Line::from(vec![
                Span::styled(label.clone(), Style::default().fg(theme.fg)),
                Span::raw("  "),
                Span::styled(hint.clone(), Style::default().fg(theme.dim)),
            ]))
        })
        .collect();

    let mut state = ListState::default();
    if !items.is_empty() {
        state.select(Some(selected.min(items.len() - 1)));
    }
    frame.render_stateful_widget(
        List::new(list_items)
            .highlight_style(theme.selection_style())
            .highlight_symbol("\u{25B6} "),
        rows[1],
        &mut state,
    );
}

/// How a decorated span looks. `ui` never sees `editor` types, so `app::view` translates
/// a `DecorationClass` into one of these — the same boundary `TreeLine` sits on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecorStyle {
    HunkRemoved,
    HunkAdded,
    HunkConflict,
    Error,
    Warning,
    Info,
    Hint,
    Match,
    MatchCurrent,
    Keyword,
    StringLit,
    Comment,
    Number,
    Type,
    Function,
}

impl DecorStyle {
    /// Which style wins where spans overlap.
    ///
    /// Agent review outranks everything: a hunk you are being asked to accept must not be
    /// hidden under syntax highlighting. Diagnostics outrank syntax for the same reason.
    fn priority(self) -> u8 {
        match self {
            DecorStyle::HunkConflict => 5,
            DecorStyle::HunkRemoved | DecorStyle::HunkAdded => 4,
            DecorStyle::MatchCurrent => 4,
            DecorStyle::Match => 3,
            DecorStyle::Error | DecorStyle::Warning => 3,
            DecorStyle::Info | DecorStyle::Hint => 2,
            _ => 1,
        }
    }
}

/// A styled range of one line, in **display columns** (see `crate::text`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpanStyle {
    pub start: usize,
    pub end: usize,
    pub style: DecorStyle,
}

/// One tab in the editor's strip.
#[derive(Debug, Clone)]
pub struct EditorTab {
    pub name: String,
    pub dirty: bool,
    pub active: bool,
}

/// One rendered editor line.
#[derive(Debug, Clone, Default)]
pub struct EditorLine {
    /// Tab-expanded display text.
    pub text: String,
    /// Decorated ranges. May overlap; the highest [`DecorStyle::priority`] wins per cell.
    pub spans: Vec<SpanStyle>,
    /// Gutter marker for a changed line or diagnostic (ASCII for low-colour terminals).
    pub marker: Option<char>,
}

impl EditorLine {
    pub fn plain(text: String) -> Self {
        Self { text, spans: Vec::new(), marker: None }
    }
}

/// Split a line into styled runs, resolving overlaps by priority.
fn styled_runs(line: &EditorLine, theme: &Theme) -> Line<'static> {
    let base = Style::default().fg(theme.fg);
    if line.spans.is_empty() {
        return Line::from(Span::styled(line.text.clone(), base));
    }

    let mut out: Vec<Span> = Vec::new();
    let mut run = String::new();
    let mut run_style: Option<DecorStyle> = None;
    let mut column = 0;

    for ch in line.text.chars() {
        // Highest-priority span covering this cell, if any. Lines are bounded by the
        // terminal width, so a scan per character is cheap and obviously correct.
        let winner = line
            .spans
            .iter()
            .filter(|s| {
                column >= s.start && (column < s.end || (s.start == s.end && column == s.start))
            })
            .max_by_key(|s| s.style.priority())
            .map(|s| s.style);

        if winner != run_style && !run.is_empty() {
            out.push(styled(std::mem::take(&mut run), run_style, theme, base));
        }
        run_style = winner;
        run.push(ch);
        // `EditorLine.text` has already been through `expand_tabs`, so `ch` is never a
        // literal tab here — the tab width used to measure it cannot matter.
        column += crate::text::display_width(&ch.to_string(), crate::text::TAB_WIDTH);
    }
    if !run.is_empty() {
        out.push(styled(run, run_style, theme, base));
    }
    Line::from(out)
}

fn styled(text: String, decor: Option<DecorStyle>, theme: &Theme, base: Style) -> Span<'static> {
    Span::styled(text, decor.map(|d| theme.decor_style(d)).unwrap_or(base))
}

/// The editor viewport: gutter, text with decorations, and a block cursor.
///
/// Takes plain strings and display columns rather than a buffer, so `ui` stays free of
/// `editor` exactly as it stays free of `filesystem` above.
#[allow(clippy::too_many_arguments)]
pub fn editor(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    focused: bool,
    tabs: &[EditorTab],
    lines: &[EditorLine],
    cursor: (usize, usize),
    scroll_top: usize,
    theme: &Theme,
) {
    let marker = if focused { " \u{25CF} " } else { " " };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.border_style(focused))
        .title(Span::styled(format!("{marker}{title} "), theme.title_style(focused)));
    let mut inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    // The tab strip takes the top row, as in ARCHITECTURE.md §6.1's layout. Only shown
    // when there is a choice to make: one file needs no strip, and the row is worth more
    // as text.
    if tabs.len() > 1 {
        let strip = Rect { height: 1, ..inner };
        frame.render_widget(Paragraph::new(tab_strip(tabs, theme)), strip);
        inner = Rect { y: inner.y + 1, height: inner.height - 1, ..inner };
        if inner.height == 0 {
            return;
        }
    }

    // Width the largest visible line number needs, so the text column does not shift as
    // you scroll past line 99. One extra cell holds the change marker.
    let last = (scroll_top + inner.height as usize).min(lines.len());
    let digits = last.max(1).to_string().len();
    // leading space + number + change marker + separating space
    let gutter_w = digits as u16 + 3;

    let rows: Vec<Line> = (scroll_top..last)
        .map(|i| {
            let line = &lines[i];
            let number = format!(" {:>digits$}", i + 1, digits = digits);
            let mark = line.marker.unwrap_or(' ');
            let mut spans = vec![
                Span::styled(number, Style::default().fg(theme.dim)),
                Span::styled(mark.to_string(), Style::default().fg(theme.accent)),
                Span::raw(" "),
            ];
            spans.extend(styled_runs(line, theme).spans);
            Line::from(spans)
        })
        .collect();
    frame.render_widget(Paragraph::new(rows), inner);

    if !focused {
        return;
    }
    let (line, column) = cursor;
    if line < scroll_top || line >= last {
        return;
    }
    // `column` is already a display column: the caller converts once, at the boundary,
    // so tabs and wide characters cannot desync the cursor from the text under it.
    let x = inner.x + gutter_w + column as u16;
    let y = inner.y + (line - scroll_top) as u16;
    if x < inner.right() && y < inner.bottom() {
        frame.set_cursor_position((x, y));
    }
}

/// The tab strip: active tab highlighted, unsaved marked.
fn tab_strip(tabs: &[EditorTab], theme: &Theme) -> Line<'static> {
    let mut spans = Vec::new();
    for (i, tab) in tabs.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled("\u{2502}", Style::default().fg(theme.border)));
        }
        let dot = if tab.dirty { " \u{2022}" } else { "" };
        let label = format!(" {}{dot} ", tab.name);
        let style =
            if tab.active { theme.selection_style() } else { Style::default().fg(theme.dim) };
        spans.push(Span::styled(label, style));
    }
    Line::from(spans)
}

/// Clamp a remembered viewport so the cursor is on screen.
///
/// The buffer owns `scroll_top` and the commands that move the cursor scroll it; this is
/// only a guard for the cases the commands could not anticipate — a terminal resize, or a
/// cursor moved by an edit rather than a motion. It never scrolls further than it must,
/// so it cannot reintroduce the "cursor pinned to one row" behaviour it replaced.
pub fn clamp_viewport(scroll_top: usize, cursor_line: usize, height: usize) -> usize {
    if height == 0 {
        return scroll_top;
    }
    if cursor_line < scroll_top {
        return cursor_line;
    }
    if cursor_line >= scroll_top + height {
        return cursor_line + 1 - height;
    }
    scroll_top
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_bar_uses_explicit_high_contrast_foregrounds() {
        use ratatui::backend::TestBackend;
        use ratatui::style::Color;
        use ratatui::Terminal;

        let theme = Theme::dark();
        let mut terminal = Terminal::new(TestBackend::new(60, 1)).unwrap();
        terminal
            .draw(|frame| status_bar(frame, frame.area(), "hints", "right", None, &theme))
            .unwrap();
        let buffer = terminal.backend().buffer();

        assert_eq!(
            buffer[(1, 0)].fg,
            Color::White,
            "left hints must not inherit a dim terminal foreground"
        );
        assert_eq!(buffer[(55, 0)].fg, Color::Gray, "right context must remain readable");
        assert_eq!(buffer[(1, 0)].bg, Color::Indexed(236));
    }

    #[test]
    fn a_cursor_already_on_screen_does_not_move_the_viewport() {
        assert_eq!(clamp_viewport(30, 35, 26), 30);
        assert_eq!(clamp_viewport(30, 30, 26), 30, "at the very top edge");
        assert_eq!(clamp_viewport(30, 55, 26), 30, "at the very bottom edge");
    }

    #[test]
    fn a_cursor_above_the_viewport_pulls_it_up() {
        assert_eq!(clamp_viewport(30, 12, 26), 12);
    }

    #[test]
    fn a_cursor_below_the_viewport_pulls_it_down_by_the_minimum() {
        assert_eq!(clamp_viewport(30, 56, 26), 31, "one line, not a jump");
    }

    #[test]
    fn a_zero_height_viewport_is_left_alone() {
        assert_eq!(clamp_viewport(7, 99, 0), 7);
    }
}
