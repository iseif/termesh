//! Stateless overlays that consume only plain view data.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};
use ratatui::Frame;

use crate::Theme;

pub struct HelpViewRow {
    pub group: String,
    pub title: String,
    pub id: String,
    pub chord: Option<String>,
}

pub struct HelpView<'a> {
    pub query: &'a str,
    pub rows: &'a [HelpViewRow],
    pub scroll: usize,
}

pub fn help(frame: &mut Frame, area: Rect, view: HelpView<'_>, theme: &Theme) {
    let rect = overlay_rect(area, 88, 88);
    frame.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border_focus))
        .title(Span::styled(" Help · Keys and Actions ", theme.title_style(true)));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    let parts = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(inner);

    let mut lines = Vec::new();
    let mut previous_group: Option<&str> = None;
    for row in view.rows {
        if previous_group != Some(row.group.as_str()) {
            if previous_group.is_some() {
                lines.push(Line::default());
            }
            lines.push(Line::styled(row.group.clone(), Style::default().fg(theme.accent)));
            previous_group = Some(&row.group);
        }
        lines.push(Line::from(vec![
            Span::styled(format!("  {:<29}", row.title), Style::default().fg(theme.fg)),
            Span::styled(
                format!("{:<13}", row.chord.as_deref().unwrap_or("palette")),
                Style::default().fg(theme.warn),
            ),
            Span::styled(row.id.clone(), Style::default().fg(theme.dim)),
        ]));
    }
    if lines.is_empty() {
        lines.push(Line::styled("No matching actions", Style::default().fg(theme.dim)));
    }
    frame.render_widget(
        Paragraph::new(lines).scroll((u16::try_from(view.scroll).unwrap_or(u16::MAX), 0)),
        parts[0],
    );

    let query = if view.query.is_empty() {
        "type to filter".to_string()
    } else {
        format!("filter: {}", view.query)
    };
    frame.render_widget(
        Paragraph::new(format!("{query} · ↑/↓ Scroll · PgUp/PgDn · Esc Close"))
            .style(Style::default().fg(theme.fg)),
        parts[1],
    );
}

pub struct GitStatusViewRow {
    pub group: &'static str,
    pub status: String,
    pub path: String,
}

pub struct GitStatusView<'a> {
    pub rows: &'a [GitStatusViewRow],
    pub selected: usize,
}

pub fn git_status(frame: &mut Frame, area: Rect, view: GitStatusView<'_>, theme: &Theme) {
    let rect = overlay_rect(area, 76, 78);
    frame.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border_focus))
        .title(Span::styled(" Git Changes ", theme.title_style(true)));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    let parts = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(inner);

    let mut items = Vec::new();
    let mut selected_item = None;
    let mut previous_group = None;
    for (row_index, row) in view.rows.iter().enumerate() {
        if previous_group != Some(row.group) {
            items.push(ListItem::new(Line::styled(row.group, Style::default().fg(theme.accent))));
            previous_group = Some(row.group);
        }
        if row_index == view.selected {
            selected_item = Some(items.len());
        }
        items.push(ListItem::new(Line::from(vec![
            Span::styled(format!("{} ", row.status), Style::default().fg(theme.fg)),
            Span::styled(row.path.clone(), Style::default().fg(theme.fg)),
        ])));
    }
    if items.is_empty() {
        items.push(ListItem::new(Line::styled(
            "Working tree clean",
            Style::default().fg(theme.dim),
        )));
    }
    let mut state = ListState::default();
    state.select(selected_item);
    frame.render_stateful_widget(
        List::new(items).highlight_style(theme.selection_style()).highlight_symbol("▶ "),
        parts[0],
        &mut state,
    );
    frame.render_widget(
        Paragraph::new("Enter Diff · s Stage · u Unstage · c Commit · b Branch · Esc Close")
            .style(Style::default().fg(theme.fg)),
        parts[1],
    );
}

pub struct GitDiffView<'a> {
    pub path: &'a str,
    pub target: &'a str,
    pub text: Option<&'a str>,
    pub truncated: bool,
    pub error: Option<&'a str>,
    /// A fact about the path rather than a failure (an untracked file has no diff).
    pub notice: Option<&'a str>,
    pub scroll: usize,
}

pub fn git_diff(frame: &mut Frame, area: Rect, view: GitDiffView<'_>, theme: &Theme) {
    let rect = overlay_rect(area, 86, 84);
    frame.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border_focus))
        .title(Span::styled(
            format!(" Git Diff · {} · {} ", view.target, view.path),
            theme.title_style(true),
        ));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    let parts = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(inner);

    let body = if let Some(error) = view.error {
        vec![Line::styled(format!("Could not load diff: {error}"), Style::default().fg(theme.warn))]
    } else if let Some(notice) = view.notice {
        // Pre-split rather than wrapped: the body Paragraph must not wrap diff lines.
        notice
            .lines()
            .map(|line| Line::styled(line.to_owned(), Style::default().fg(theme.dim)))
            .collect()
    } else if let Some(text) = view.text {
        if text.is_empty() {
            vec![Line::styled(
                "No textual diff (the file may be binary or unchanged).",
                Style::default().fg(theme.dim),
            )]
        } else {
            text.lines()
                .map(|line| {
                    let style = if line.starts_with('+') && !line.starts_with("+++") {
                        theme.decor_style(crate::widgets::DecorStyle::HunkAdded)
                    } else if line.starts_with('-') && !line.starts_with("---") {
                        theme.decor_style(crate::widgets::DecorStyle::HunkRemoved)
                    } else if line.starts_with("@@") {
                        Style::default().fg(theme.accent)
                    } else {
                        Style::default().fg(theme.fg)
                    };
                    Line::styled(line.to_owned(), style)
                })
                .collect()
        }
    } else {
        vec![Line::styled("Loading diff…", Style::default().fg(theme.dim))]
    };
    frame.render_widget(
        Paragraph::new(body).scroll((u16::try_from(view.scroll).unwrap_or(u16::MAX), 0)),
        parts[0],
    );
    let truncated = if view.truncated { " · truncated" } else { "" };
    frame.render_widget(
        Paragraph::new(format!(
            "{} diff{truncated} · ↑/↓ Scroll · PgUp/PgDn · Esc Back",
            view.target
        ))
        .style(Style::default().fg(theme.fg)),
        parts[1],
    );
}

fn overlay_rect(area: Rect, width: u16, height: u16) -> Rect {
    if area.width < 80 {
        Rect::new(
            area.x.saturating_add(1),
            area.y.saturating_add(1),
            area.width.saturating_sub(2),
            area.height.saturating_sub(2),
        )
    } else {
        crate::centered_rect(width, height, area)
    }
}

pub struct GitBranchesView<'a> {
    pub items: &'a [String],
    pub selected: usize,
}

pub fn git_branches(frame: &mut Frame, area: Rect, view: GitBranchesView<'_>, theme: &Theme) {
    let rect = overlay_rect(area, 60, 60);
    frame.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border_focus))
        .title(Span::styled(" Switch Branch ", theme.title_style(true)));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    let parts = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(inner);
    let items = view
        .items
        .iter()
        .map(|item| ListItem::new(Line::styled(item.clone(), Style::default().fg(theme.fg))))
        .collect::<Vec<_>>();
    let mut state = ListState::default();
    if !view.items.is_empty() {
        state.select(Some(view.selected.min(view.items.len() - 1)));
    }
    frame.render_stateful_widget(
        List::new(items).highlight_style(theme.selection_style()).highlight_symbol("▶ "),
        parts[0],
        &mut state,
    );
    frame.render_widget(
        Paragraph::new("Enter Switch · Esc Close").style(Style::default().fg(theme.fg)),
        parts[1],
    );
}

pub struct SearchView<'a> {
    pub title: &'a str,
    pub query: &'a str,
    pub items: &'a [String],
    pub selected: usize,
    /// Live state — "12 results", "searching…". Empty when the surface has none.
    pub status: &'a str,
    /// Key hints. Separate from `status` because this list reuses the widget: the task
    /// picker's keys are not the search overlay's, and appending a fixed set here is
    /// how the picker ended up advertising both.
    pub hints: &'a str,
    pub preview: Option<(&'a str, usize)>,
}

pub fn search(frame: &mut Frame, area: Rect, view: SearchView<'_>, theme: &Theme) {
    let rect = if area.width < 60 {
        Rect::new(
            area.x.saturating_add(1),
            area.y.saturating_add(1),
            area.width.saturating_sub(2),
            area.height.saturating_sub(2),
        )
    } else {
        crate::centered_rect(70, 70, area)
    };
    frame.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border_focus))
        .title(Span::styled(format!(" {} ", view.title), theme.title_style(true)));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1), Constraint::Length(1)])
        .split(inner);

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("› ", Style::default().fg(theme.accent)),
            Span::styled(view.query, Style::default().fg(theme.fg)),
            Span::styled("▁", Style::default().fg(theme.dim)),
        ])),
        rows[0],
    );
    let list_items: Vec<ListItem> = view
        .items
        .iter()
        .map(|item| ListItem::new(Line::styled(item.clone(), Style::default().fg(theme.fg))))
        .collect();
    let mut state = ListState::default();
    if !view.items.is_empty() {
        state.select(Some(view.selected.min(view.items.len() - 1)));
    }
    let list =
        List::new(list_items).highlight_style(theme.selection_style()).highlight_symbol("▶ ");
    if let Some((preview, start_line)) = view.preview {
        let panes = if rect.width >= 80 {
            Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
                .split(rows[1])
        } else {
            Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(1), Constraint::Length(5)])
                .split(rows[1])
        };
        frame.render_stateful_widget(list, panes[0], &mut state);
        let numbered = preview
            .lines()
            .enumerate()
            .map(|(index, line)| format!("{:>4} {line}", start_line + index))
            .collect::<Vec<_>>()
            .join("\n");
        frame.render_widget(
            Paragraph::new(numbered)
                .block(Block::default().borders(Borders::LEFT).title(" Preview "))
                .style(Style::default().fg(theme.fg)),
            panes[1],
        );
    } else {
        frame.render_stateful_widget(list, rows[1], &mut state);
    }
    let footer = if view.status.is_empty() {
        view.hints.to_string()
    } else {
        format!("{} · {}", view.status, view.hints)
    };
    frame.render_widget(
        Paragraph::new(Line::styled(footer, Style::default().fg(theme.fg))),
        rows[2],
    );
}

pub struct ProblemsView<'a> {
    pub items: &'a [String],
    pub selected: usize,
}

pub fn problems(frame: &mut Frame, area: Rect, view: ProblemsView<'_>, theme: &Theme) {
    let rect = if area.width < 60 {
        Rect::new(
            area.x.saturating_add(1),
            area.y.saturating_add(1),
            area.width.saturating_sub(2),
            area.height.saturating_sub(2),
        )
    } else {
        crate::centered_rect(80, 70, area)
    };
    frame.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border_focus))
        .title(Span::styled(" Problems ", theme.title_style(true)));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(inner);
    let items = view
        .items
        .iter()
        .map(|item| ListItem::new(Line::styled(item.clone(), Style::default().fg(theme.fg))))
        .collect::<Vec<_>>();
    let mut state = ListState::default();
    if !view.items.is_empty() {
        state.select(Some(view.selected.min(view.items.len() - 1)));
    }
    frame.render_stateful_widget(
        List::new(items).highlight_style(theme.selection_style()).highlight_symbol("▶ "),
        rows[0],
        &mut state,
    );
    frame.render_widget(
        Paragraph::new("Enter Open · F8 Next · Shift+F8 Previous · Esc Close")
            .style(Style::default().fg(theme.fg)),
        rows[1],
    );
}

pub struct HoverView<'a> {
    pub text: &'a str,
    pub truncated: bool,
}

pub fn hover(
    frame: &mut Frame,
    editor: Rect,
    cursor: (u16, u16),
    view: HoverView<'_>,
    theme: &Theme,
) {
    let line_count = view.text.lines().count().clamp(1, 10) as u16;
    let width = view
        .text
        .lines()
        .map(|line| line.chars().count())
        .max()
        .unwrap_or(1)
        .saturating_add(4)
        .clamp(18, 60) as u16;
    let rect = crate::cursor_anchored_rect(editor, cursor, width, line_count + 2);
    frame.render_widget(Clear, rect);
    let suffix = if view.truncated { " · truncated" } else { "" };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border_focus))
        .title(Span::styled(format!(" Hover{suffix} "), theme.title_style(true)));
    frame.render_widget(
        Paragraph::new(view.text).block(block).style(Style::default().fg(theme.fg)),
        rect,
    );
}

pub struct SelectionView<'a> {
    pub title: &'a str,
    pub items: &'a [String],
    pub selected: usize,
    pub footer: &'a str,
}

pub fn selection(frame: &mut Frame, area: Rect, view: SelectionView<'_>, theme: &Theme) {
    let rect = overlay_rect(area, 72, 68);
    selection_in(frame, rect, view, theme);
}

pub fn completion(
    frame: &mut Frame,
    editor: Rect,
    cursor: (u16, u16),
    view: SelectionView<'_>,
    theme: &Theme,
) {
    let height = (view.items.len() as u16).saturating_add(3).clamp(4, 12);
    let rect = crate::cursor_anchored_rect(editor, cursor, 52, height);
    selection_in(frame, rect, view, theme);
}

pub fn references(frame: &mut Frame, area: Rect, view: SelectionView<'_>, theme: &Theme) {
    selection(frame, area, view, theme);
}

pub fn symbols(frame: &mut Frame, area: Rect, view: SelectionView<'_>, theme: &Theme) {
    selection(frame, area, view, theme);
}

fn selection_in(frame: &mut Frame, rect: Rect, view: SelectionView<'_>, theme: &Theme) {
    frame.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border_focus))
        .title(Span::styled(format!(" {} ", view.title), theme.title_style(true)));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(inner);
    let items = view
        .items
        .iter()
        .map(|item| ListItem::new(Line::styled(item.clone(), Style::default().fg(theme.fg))))
        .collect::<Vec<_>>();
    let mut state = ListState::default();
    if !view.items.is_empty() {
        state.select(Some(view.selected.min(view.items.len() - 1)));
    }
    frame.render_stateful_widget(
        List::new(items).highlight_style(theme.selection_style()).highlight_symbol("▶ "),
        rows[0],
        &mut state,
    );
    frame.render_widget(Paragraph::new(view.footer).style(Style::default().fg(theme.fg)), rows[1]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    /// Render the footer row of a `search` overlay as text.
    fn search_footer(status: &str, hints: &str) -> String {
        let theme = Theme::dark();
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal
            .draw(|frame| {
                search(
                    frame,
                    frame.area(),
                    SearchView {
                        title: "Run Task",
                        query: "",
                        items: &[],
                        selected: 0,
                        status,
                        hints,
                        preview: None,
                    },
                    &theme,
                );
            })
            .unwrap();
        let rect = crate::centered_rect(70, 70, Rect::new(0, 0, 100, 30));
        let row = rect.y + rect.height - 2;
        let buffer = terminal.backend().buffer();
        (rect.x + 1..rect.x + rect.width - 1)
            .map(|x| buffer[(x, row)].symbol().to_string())
            .collect::<String>()
            .trim_end()
            .to_string()
    }

    #[test]
    fn a_surface_with_no_status_shows_only_its_own_key_hints() {
        // The task picker reuses this widget, and the footer used to append a fixed
        // "Enter Open · Esc Close" to whatever it was given — so the picker advertised
        // both its keys and the search overlay's.
        let footer = search_footer("", "Enter Run · Esc Close");
        assert_eq!(footer, "Enter Run · Esc Close");
    }

    #[test]
    fn a_surface_with_a_status_shows_it_before_its_hints() {
        let footer = search_footer("12 results", "Enter Open · Esc Close");
        assert_eq!(footer, "12 results · Enter Open · Esc Close");
    }

    #[test]
    fn search_footer_uses_the_readable_foreground() {
        let theme = Theme::dark();
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal
            .draw(|frame| {
                search(
                    frame,
                    frame.area(),
                    SearchView {
                        title: "Quick Open",
                        query: "model",
                        items: &[],
                        selected: 0,
                        status: "Ready",
                        hints: "Enter Open · Esc Close",
                        preview: None,
                    },
                    &theme,
                );
            })
            .unwrap();

        let rect = crate::centered_rect(70, 70, Rect::new(0, 0, 100, 30));
        let footer = (rect.x + 1, rect.y + rect.height - 2);
        let cell = &terminal.backend().buffer()[footer];
        assert_eq!(cell.symbol(), "R");
        assert_eq!(cell.fg, theme.fg);
    }
}
