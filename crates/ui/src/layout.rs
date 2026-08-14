//! Pane identity + the tiling layout engine (ARCHITECTURE.md §6.1, §7.1).
use ratatui::layout::{Constraint, Direction, Layout, Rect};

/// The four structural regions. The Agent pane is a first-class peer (§3, §6.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pane {
    Project,
    Editor,
    Terminal,
    Agent,
}

impl Pane {
    pub const ORDER: [Pane; 4] = [Pane::Project, Pane::Editor, Pane::Terminal, Pane::Agent];

    pub fn title(self) -> &'static str {
        match self {
            Pane::Project => "Project",
            Pane::Editor => "Editor",
            Pane::Terminal => "Terminal",
            Pane::Agent => "Agent",
        }
    }

    fn index(self) -> usize {
        Self::ORDER.iter().position(|p| *p == self).unwrap()
    }
    pub fn next(self) -> Self {
        Self::ORDER[(self.index() + 1) % 4]
    }
    pub fn prev(self) -> Self {
        Self::ORDER[(self.index() + 3) % 4]
    }
}

/// Split ratios, adjustable at runtime (resize commands). Percentages, clamped.
#[derive(Debug, Clone, Copy)]
pub struct LayoutState {
    pub sidebar_pct: u16,
    pub bottom_pct: u16,
    pub agent_pct: u16,
}

impl Default for LayoutState {
    fn default() -> Self {
        Self { sidebar_pct: 22, bottom_pct: 32, agent_pct: 26 }
    }
}

impl LayoutState {
    fn clamp(v: u16) -> u16 {
        v.clamp(10, 45)
    }
    pub fn grow_sidebar(&mut self) {
        self.sidebar_pct = Self::clamp(self.sidebar_pct + 3);
    }
    pub fn shrink_sidebar(&mut self) {
        self.sidebar_pct = Self::clamp(self.sidebar_pct.saturating_sub(3));
    }
    pub fn grow_bottom(&mut self) {
        self.bottom_pct = Self::clamp(self.bottom_pct + 3);
    }
    pub fn shrink_bottom(&mut self) {
        self.bottom_pct = Self::clamp(self.bottom_pct.saturating_sub(3));
    }
}

/// Computed rectangles for one frame.
#[derive(Debug, Clone, Copy)]
pub struct Regions {
    pub project: Rect,
    pub editor: Rect,
    pub terminal: Rect,
    pub agent: Rect,
    pub status: Rect,
}

pub fn regions(area: Rect, st: &LayoutState) -> Regions {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(area);
    let body = vertical[0];
    let status = vertical[1];

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(st.sidebar_pct),
            Constraint::Min(10),
            Constraint::Percentage(st.agent_pct),
        ])
        .split(body);

    let center = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Percentage(st.bottom_pct)])
        .split(cols[1]);

    Regions { project: cols[0], editor: center[0], terminal: center[1], agent: cols[2], status }
}

/// A centered rectangle sized as a percentage of `area`.
pub fn centered_rect(pct_x: u16, pct_y: u16, area: Rect) -> Rect {
    let v = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - pct_y) / 2),
            Constraint::Percentage(pct_y),
            Constraint::Percentage((100 - pct_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - pct_x) / 2),
            Constraint::Percentage(pct_x),
            Constraint::Percentage((100 - pct_x) / 2),
        ])
        .split(v[1])[1]
}

/// Place a fixed-size tooltip beside a cursor, clamped to `area` and flipped above
/// when there is not enough room below.
pub fn cursor_anchored_rect(area: Rect, cursor: (u16, u16), width: u16, height: u16) -> Rect {
    let width = width.min(area.width).max(1);
    let height = height.min(area.height).max(1);
    let max_x = area.right().saturating_sub(width);
    let x = cursor.0.saturating_add(1).min(max_x).max(area.x);
    let below = cursor.1.saturating_add(1);
    let y = if below.saturating_add(height) <= area.bottom() {
        below
    } else {
        cursor.1.saturating_sub(height).max(area.y)
    };
    Rect::new(x, y, width, height)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cursor_tooltip_flips_above_when_below_would_overflow() {
        let area = Rect::new(10, 5, 40, 12);
        let below = cursor_anchored_rect(area, (20, 7), 18, 4);
        assert!(below.y > 7);
        let above = cursor_anchored_rect(area, (20, 15), 18, 4);
        assert!(above.y < 15);
        assert!(above.x >= area.x && above.right() <= area.right());
    }
}
