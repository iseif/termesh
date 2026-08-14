//! Terminal colour-capability detection. Detection consumes an injected environment map
//! so tests never mutate process-global environment variables.

use std::collections::HashMap;

/// How much colour the terminal can faithfully display.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorDepth {
    TrueColor,
    Indexed256,
    Ansi16,
    None,
}

/// Detect capability from an environment snapshot, in precedence order.
pub fn detect_color_depth(environment: &HashMap<String, String>) -> ColorDepth {
    if environment.contains_key("NO_COLOR") {
        return ColorDepth::None;
    }

    if environment
        .get("COLORTERM")
        .is_some_and(|value| matches!(value.to_ascii_lowercase().as_str(), "truecolor" | "24bit"))
    {
        return ColorDepth::TrueColor;
    }

    if environment.get("TERM").is_some_and(|value| value.to_ascii_lowercase().contains("256color"))
    {
        return ColorDepth::Indexed256;
    }

    ColorDepth::Ansi16
}

/// Snapshot the real process environment once, then use the pure detector above.
pub fn current_color_depth() -> ColorDepth {
    let environment = std::env::vars_os()
        .filter_map(|(key, value)| {
            Some((key.into_string().ok()?, value.to_string_lossy().into_owned()))
        })
        .collect();
    detect_color_depth(&environment)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn env(entries: &[(&str, &str)]) -> HashMap<String, String> {
        entries.iter().map(|(key, value)| ((*key).into(), (*value).into())).collect()
    }

    #[test]
    fn no_color_wins_over_everything() {
        assert_eq!(
            detect_color_depth(&env(&[("NO_COLOR", "1"), ("COLORTERM", "truecolor")])),
            ColorDepth::None
        );
    }

    #[test]
    fn colorterm_selects_truecolor_and_term_selects_256() {
        assert_eq!(detect_color_depth(&env(&[("COLORTERM", "truecolor")])), ColorDepth::TrueColor);
        assert_eq!(detect_color_depth(&env(&[("COLORTERM", "24bit")])), ColorDepth::TrueColor);
        assert_eq!(detect_color_depth(&env(&[("TERM", "xterm-256color")])), ColorDepth::Indexed256);
        assert_eq!(detect_color_depth(&env(&[("TERM", "xterm")])), ColorDepth::Ansi16);
    }
}
