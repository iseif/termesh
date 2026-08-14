//! Tree-sitter highlighting (ARCHITECTURE.md Appendix A).
//!
//! Produces plain `(start, end, SyntaxKind)` spans in **char** offsets, which the editor
//! turns into decorations. Nothing tree-sitter crosses this boundary, so adding a
//! language is a table entry here and touches nothing above.
//!
//! **Reparses whole.** ARCHITECTURE.md §8 wants incremental parsing fed by the change
//! stream, and the transaction spine already carries everything needed to do it — the
//! missing piece is keeping the `Tree` and calling `Tree::edit` with each change. A full
//! reparse of an ordinary source file is well under a millisecond and happens on edit
//! rather than on render, so this is a real but bounded shortcut, and the seam for fixing
//! it is [`Highlighter::highlight`].
#![forbid(unsafe_code)]

use std::path::Path;

use termesh_editor::SyntaxKind;
use tree_sitter_highlight::{
    Highlight, HighlightConfiguration, HighlightEvent, Highlighter as TsHighlighter,
};

/// A language we can highlight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    Rust,
}

impl Language {
    /// The language for a file, by extension.
    ///
    /// Rust only for now: ARCHITECTURE.md §14 makes Rust the flagship and says more
    /// languages arrive as recipes, so this is the table those recipes extend.
    pub fn from_path(path: &Path) -> Option<Self> {
        match path.extension()?.to_str()? {
            "rs" => Some(Language::Rust),
            _ => None,
        }
    }
}

/// The capture names we ask tree-sitter for, in the order their indices are assigned.
///
/// Deliberately short. A highlighter with forty token classes needs a theme with forty
/// colours, and a terminal has far fewer than that to spend legibly.
const CAPTURES: &[(&str, SyntaxKind)] = &[
    ("keyword", SyntaxKind::Keyword),
    ("string", SyntaxKind::StringLit),
    ("comment", SyntaxKind::Comment),
    ("number", SyntaxKind::Number),
    ("type", SyntaxKind::Type),
    ("function", SyntaxKind::Function),
    // Aliases the Rust queries actually emit, folded onto the same kinds.
    ("constructor", SyntaxKind::Type),
    ("type.builtin", SyntaxKind::Type),
    ("function.method", SyntaxKind::Function),
    ("function.macro", SyntaxKind::Function),
    ("constant", SyntaxKind::Number),
    ("constant.builtin", SyntaxKind::Number),
    ("escape", SyntaxKind::StringLit),
];

/// A highlighted span, in char offsets.
pub type Span = (usize, usize, SyntaxKind);

/// Parses and highlights one language.
pub struct Highlighter {
    inner: TsHighlighter,
    config: HighlightConfiguration,
}

impl Highlighter {
    /// Build a highlighter, or `None` if the grammar and its queries disagree — a
    /// mismatch between the grammar crate and its query file is a packaging problem, and
    /// an editor that refuses to open a file over it would be worse than one that shows
    /// it unhighlighted.
    pub fn new(language: Language) -> Option<Self> {
        let names: Vec<&str> = CAPTURES.iter().map(|(name, _)| *name).collect();

        let mut config = match language {
            Language::Rust => HighlightConfiguration::new(
                tree_sitter_rust::LANGUAGE.into(),
                "rust",
                tree_sitter_rust::HIGHLIGHTS_QUERY,
                "",
                "",
            )
            .ok()?,
        };
        config.configure(&names);

        Some(Self { inner: TsHighlighter::new(), config })
    }

    /// Highlight `text`.
    ///
    /// Returns spans in char offsets, non-overlapping and in document order. On any parse
    /// failure the answer is "no highlighting" rather than an error: unhighlighted code is
    /// perfectly editable, and half-coloured code from a partial parse is not better.
    pub fn highlight(&mut self, text: &str) -> Vec<Span> {
        let Ok(events) = self.inner.highlight(&self.config, text.as_bytes(), None, |_| None) else {
            return Vec::new();
        };

        // tree-sitter works in bytes; everything above works in chars (ADR-0006 §1).
        // Building the prefix table once beats counting per span.
        let char_at = ByteToChar::new(text);

        let mut spans = Vec::new();
        let mut stack: Vec<Highlight> = Vec::new();
        for event in events.flatten() {
            match event {
                HighlightEvent::HighlightStart(h) => stack.push(h),
                HighlightEvent::HighlightEnd => {
                    stack.pop();
                }
                HighlightEvent::Source { start, end } => {
                    // The innermost capture wins, which is what nesting means.
                    if let Some(kind) = stack.last().and_then(|h| kind_of(*h)) {
                        if start < end {
                            spans.push((char_at.get(start), char_at.get(end), kind));
                        }
                    }
                }
            }
        }
        spans
    }
}

fn kind_of(highlight: Highlight) -> Option<SyntaxKind> {
    CAPTURES.get(highlight.0).map(|(_, kind)| *kind)
}

/// Byte offset → char offset, precomputed.
struct ByteToChar {
    /// `chars[b]` is the number of chars before byte `b`.
    chars: Vec<usize>,
}

impl ByteToChar {
    fn new(text: &str) -> Self {
        let mut chars = vec![0; text.len() + 1];
        let mut count = 0;
        for (byte, _) in text.char_indices() {
            chars[byte] = count;
            count += 1;
        }
        // Every trailing byte of a multi-byte char, and the end, map to the running total.
        let mut last = 0;
        for slot in chars.iter_mut() {
            if *slot == 0 && last != 0 {
                *slot = last;
            } else {
                last = *slot;
            }
        }
        chars[text.len()] = count;
        Self { chars }
    }

    fn get(&self, byte: usize) -> usize {
        self.chars.get(byte).copied().unwrap_or_else(|| self.chars.last().copied().unwrap_or(0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn highlight(source: &str) -> Vec<Span> {
        Highlighter::new(Language::Rust).expect("the Rust grammar loads").highlight(source)
    }

    /// The span for the first occurrence of `text`, if it was highlighted.
    fn kind_of_word(source: &str, word: &str) -> Option<SyntaxKind> {
        let at = source.find(word).expect("the word is in the source");
        let start = source[..at].chars().count();
        highlight(source).into_iter().find(|(s, e, _)| *s <= start && start < *e).map(|(_, _, k)| k)
    }

    #[test]
    fn the_rust_grammar_loads() {
        assert!(Highlighter::new(Language::Rust).is_some());
    }

    #[test]
    fn a_language_is_chosen_by_extension() {
        assert_eq!(Language::from_path(Path::new("src/main.rs")), Some(Language::Rust));
        assert_eq!(Language::from_path(Path::new("README.md")), None);
        assert_eq!(Language::from_path(Path::new("noextension")), None);
    }

    #[test]
    fn keywords_comments_and_strings_are_distinguished() {
        let source = "// a note\nfn main() {\n    let s = \"hello\";\n}\n";
        assert_eq!(kind_of_word(source, "// a note"), Some(SyntaxKind::Comment));
        assert_eq!(kind_of_word(source, "fn"), Some(SyntaxKind::Keyword));
        assert_eq!(kind_of_word(source, "\"hello\""), Some(SyntaxKind::StringLit));
    }

    #[test]
    fn numbers_are_highlighted() {
        assert_eq!(kind_of_word("fn f() { let x = 42; }", "42"), Some(SyntaxKind::Number));
    }

    #[test]
    fn spans_are_char_offsets_not_byte_offsets() {
        // The comment holds a multi-byte character, so every span after it would be
        // wrong if these were byte offsets.
        let source = "// héllo\nfn main() {}\n";
        let at = source.find("fn").unwrap();
        assert_ne!(at, source[..at].chars().count(), "the test is only meaningful if they differ");

        let start = source[..at].chars().count();
        assert!(
            highlight(source).iter().any(|(s, _, k)| *s == start && *k == SyntaxKind::Keyword),
            "`fn` should be highlighted at its char offset"
        );
    }

    #[test]
    fn spans_never_run_past_the_end_of_the_text() {
        let source = "fn main() {}\n";
        let chars = source.chars().count();
        assert!(highlight(source).iter().all(|(_, end, _)| *end <= chars));
    }

    #[test]
    fn spans_come_back_in_document_order() {
        let spans = highlight("// one\nfn two() {}\n// three\n");
        assert!(spans.windows(2).all(|w| w[0].0 <= w[1].0), "got {spans:?}");
    }

    #[test]
    fn empty_and_broken_input_produce_no_highlighting_rather_than_an_error() {
        assert!(highlight("").is_empty());
        // Unparseable code is still editable; half-coloured is not better than plain.
        let _ = highlight("fn fn fn ((( unclosed");
    }
}
