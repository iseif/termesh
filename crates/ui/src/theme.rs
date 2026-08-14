//! Named color/style tokens. Widgets reference these, never raw colors, so a future
//! theme file just swaps the token values (ARCHITECTURE.md §6, §13).
use ratatui::style::{Color, Modifier, Style};
use termesh_platform::ColorDepth;

#[derive(Debug, Clone)]
pub struct Theme {
    depth: ColorDepth,
    pub fg: Color,
    pub dim: Color,
    pub accent: Color,
    pub border: Color,
    pub border_focus: Color,
    pub sel_bg: Color,
    pub sel_fg: Color,
    pub statusbar_bg: Color,
    pub statusbar_fg: Color,
    pub statusbar_dim: Color,
    pub warn: Color,
}

impl Theme {
    pub fn dark() -> Self {
        Self {
            depth: ColorDepth::Indexed256,
            // The terminal's own foreground, not a colour we picked. A hardcoded grey
            // washes out on a dark background and all but disappears on a light one —
            // and a terminal-native editor does not get to assume which the user has.
            fg: Color::Reset,
            dim: Color::DarkGray,
            accent: Color::Magenta,
            border: Color::DarkGray,
            border_focus: Color::Magenta,
            sel_bg: Color::Magenta,
            sel_fg: Color::Black,
            statusbar_bg: Color::Indexed(236),
            // Unlike pane text, a status bar has an app-chosen background. Explicit
            // foregrounds keep it readable when the terminal's Reset/DarkGray palette
            // is close to xterm color 236.
            statusbar_fg: Color::White,
            statusbar_dim: Color::Gray,
            warn: Color::Yellow,
        }
    }

    /// Select the compiled dark palette at the terminal's actual colour depth.
    pub fn for_depth(depth: ColorDepth) -> Self {
        let mut theme = Self::dark();
        theme.depth = depth;

        if depth == ColorDepth::Ansi16 {
            // The always-visible status background is the palette's one 256-colour
            // token. Reset is more faithful than asking a 16-colour terminal to guess.
            theme.statusbar_bg = Color::Reset;
        } else if depth == ColorDepth::None {
            theme.fg = Color::Reset;
            theme.dim = Color::Reset;
            theme.accent = Color::Reset;
            theme.border = Color::Reset;
            theme.border_focus = Color::Reset;
            theme.sel_bg = Color::Reset;
            theme.sel_fg = Color::Reset;
            theme.statusbar_bg = Color::Reset;
            theme.statusbar_fg = Color::Reset;
            theme.statusbar_dim = Color::Reset;
            theme.warn = Color::Reset;
        }

        theme
    }

    pub fn depth(&self) -> ColorDepth {
        self.depth
    }

    /// Every colour the compiled theme can emit. This is deliberately exhaustive so
    /// degradation tests audit the token surface instead of spot-checking one widget.
    pub fn all_colors(&self) -> Vec<Color> {
        let mut colors = vec![
            self.fg,
            self.dim,
            self.accent,
            self.border,
            self.border_focus,
            self.sel_bg,
            self.sel_fg,
            self.statusbar_bg,
            self.statusbar_fg,
            self.statusbar_dim,
            self.warn,
        ];
        for decor in [
            crate::widgets::DecorStyle::HunkRemoved,
            crate::widgets::DecorStyle::HunkAdded,
            crate::widgets::DecorStyle::HunkConflict,
            crate::widgets::DecorStyle::Error,
            crate::widgets::DecorStyle::Warning,
            crate::widgets::DecorStyle::Info,
            crate::widgets::DecorStyle::Hint,
            crate::widgets::DecorStyle::Match,
            crate::widgets::DecorStyle::MatchCurrent,
            crate::widgets::DecorStyle::Keyword,
            crate::widgets::DecorStyle::StringLit,
            crate::widgets::DecorStyle::Comment,
            crate::widgets::DecorStyle::Number,
            crate::widgets::DecorStyle::Type,
            crate::widgets::DecorStyle::Function,
        ] {
            let style = self.decor_style(decor);
            colors.extend(style.fg);
            colors.extend(style.bg);
        }
        for color in [
            crate::widgets::TerminalColor::Indexed(1),
            crate::widgets::TerminalColor::Indexed(180),
            crate::widgets::TerminalColor::Rgb(1, 2, 3),
        ] {
            colors.push(self.terminal_color(color));
        }
        colors
    }

    fn adapt_color(&self, color: Color) -> Color {
        match self.depth {
            ColorDepth::TrueColor | ColorDepth::Indexed256 => color,
            ColorDepth::Ansi16 => match color {
                Color::Indexed(index @ 0..=15) => ANSI_COLORS[index as usize],
                Color::Indexed(_) | Color::Rgb(..) => Color::Reset,
                color => color,
            },
            ColorDepth::None => Color::Reset,
        }
    }

    pub fn border_style(&self, focused: bool) -> Style {
        if focused {
            Style::default().fg(self.border_focus).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(self.border)
        }
    }

    pub fn title_style(&self, focused: bool) -> Style {
        if focused {
            Style::default().fg(self.accent).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(self.dim)
        }
    }

    /// How a decorated span looks. One place, so a theme file can swap all of them.
    pub fn decor_style(&self, decor: crate::widgets::DecorStyle) -> Style {
        use crate::widgets::DecorStyle::*;
        match decor {
            // Agent review, styled like a diff so it reads without a legend.
            HunkRemoved => Style::default()
                .fg(self.adapt_color(Color::Red))
                .add_modifier(Modifier::CROSSED_OUT),
            HunkAdded => Style::default().fg(self.adapt_color(Color::Green)),
            // A conflicted hunk must not look like one you can just accept.
            HunkConflict => Style::default()
                .fg(self.adapt_color(Color::Black))
                .bg(self.adapt_color(Color::Yellow))
                .add_modifier(Modifier::BOLD),
            Match => Style::default()
                .fg(self.adapt_color(Color::Black))
                .bg(self.adapt_color(Color::Indexed(180))),
            // The one you are on has to be findable at a glance among the rest.
            MatchCurrent => Style::default()
                .fg(self.adapt_color(Color::Black))
                .bg(self.adapt_color(Color::Yellow))
                .add_modifier(Modifier::BOLD),
            Error => {
                Style::default().fg(self.adapt_color(Color::Red)).add_modifier(Modifier::UNDERLINED)
            }
            Warning => Style::default().fg(self.warn).add_modifier(Modifier::UNDERLINED),
            Info => Style::default().fg(self.accent).add_modifier(Modifier::UNDERLINED),
            Hint => Style::default().fg(self.dim).add_modifier(Modifier::UNDERLINED),
            Keyword => Style::default().fg(self.adapt_color(Color::Magenta)),
            StringLit => Style::default().fg(self.adapt_color(Color::Green)),
            Comment => Style::default().fg(self.dim).add_modifier(Modifier::ITALIC),
            Number => Style::default().fg(self.adapt_color(Color::Cyan)),
            Type => Style::default().fg(self.adapt_color(Color::Yellow)),
            Function => Style::default().fg(self.adapt_color(Color::Blue)),
        }
    }

    pub fn selection_style(&self) -> Style {
        Style::default().bg(self.sel_bg).fg(self.sel_fg).add_modifier(Modifier::BOLD)
    }

    pub fn terminal_color(&self, color: crate::widgets::TerminalColor) -> Color {
        use crate::widgets::TerminalColor;
        let color = match color {
            TerminalColor::Default => Color::Reset,
            TerminalColor::Rgb(r, g, b) => Color::Rgb(r, g, b),
            TerminalColor::Indexed(index @ 0..=15) => ANSI_COLORS[index as usize],
            TerminalColor::Indexed(index @ 16..=231) => {
                let index = index - 16;
                let levels = [0, 95, 135, 175, 215, 255];
                Color::Rgb(
                    levels[(index / 36) as usize],
                    levels[((index / 6) % 6) as usize],
                    levels[(index % 6) as usize],
                )
            }
            TerminalColor::Indexed(index) => {
                let value = 8 + (index - 232) * 10;
                Color::Rgb(value, value, value)
            }
        };
        self.adapt_color(color)
    }
}

const ANSI_COLORS: [Color; 16] = [
    Color::Black,
    Color::Red,
    Color::Green,
    Color::Yellow,
    Color::Blue,
    Color::Magenta,
    Color::Cyan,
    Color::Gray,
    Color::DarkGray,
    Color::LightRed,
    Color::LightGreen,
    Color::LightYellow,
    Color::LightBlue,
    Color::LightMagenta,
    Color::LightCyan,
    Color::White,
];

impl Default for Theme {
    fn default() -> Self {
        Self::dark()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::widgets::{DecorStyle, TerminalColor};
    use termesh_platform::ColorDepth;

    #[test]
    fn the_sixteen_color_theme_uses_no_indexed_color_above_fifteen() {
        for color in Theme::for_depth(ColorDepth::Ansi16).all_colors() {
            assert!(!matches!(color, Color::Indexed(i) if i > 15), "{color:?} needs 256 colors");
            assert!(!matches!(color, Color::Rgb(..)), "{color:?} needs truecolor");
        }
    }

    #[test]
    fn the_no_color_theme_resets_every_declared_color() {
        assert!(Theme::for_depth(ColorDepth::None)
            .all_colors()
            .into_iter()
            .all(|color| color == Color::Reset));
    }

    #[test]
    fn info_and_hint_have_distinct_underlined_styles() {
        let theme = Theme::dark();
        let info = theme.decor_style(DecorStyle::Info);
        let hint = theme.decor_style(DecorStyle::Hint);
        assert!(info.add_modifier.contains(Modifier::UNDERLINED));
        assert!(hint.add_modifier.contains(Modifier::UNDERLINED));
        assert_ne!(info, theme.decor_style(DecorStyle::Warning));
        assert_ne!(hint, theme.decor_style(DecorStyle::Warning));
    }

    #[test]
    fn terminal_colors_follow_the_xterm_palette() {
        let theme = Theme::dark();
        assert_eq!(theme.terminal_color(TerminalColor::Default), Color::Reset);
        assert_eq!(theme.terminal_color(TerminalColor::Indexed(1)), Color::Red);
        assert_eq!(theme.terminal_color(TerminalColor::Indexed(9)), Color::LightRed);
        assert_eq!(theme.terminal_color(TerminalColor::Indexed(16)), Color::Rgb(0, 0, 0));
        assert_eq!(theme.terminal_color(TerminalColor::Indexed(231)), Color::Rgb(255, 255, 255));
        assert_eq!(theme.terminal_color(TerminalColor::Indexed(232)), Color::Rgb(8, 8, 8));
        assert_eq!(theme.terminal_color(TerminalColor::Indexed(255)), Color::Rgb(238, 238, 238));
        assert_eq!(theme.terminal_color(TerminalColor::Rgb(1, 2, 3)), Color::Rgb(1, 2, 3));
    }
}
