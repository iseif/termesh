//! Finding text in a buffer. Pure functions over a rope, in char offsets.
//!
//! Literal substring matching, not regex: ARCHITECTURE.md §14 puts find/replace in the
//! MVP and regex nowhere, and a literal search is what people reach for when they are
//! looking at a symbol name. Regex belongs with the ripgrep-backed workspace search in
//! Phase 05, where the engine already exists.

use ropey::Rope;

/// A match, as a char range.
pub type Match = (usize, usize);

/// Whether a search distinguishes case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CaseMode {
    /// Case-insensitive until the query contains an uppercase letter — the "smart case"
    /// behaviour people expect without having to reach for a toggle.
    #[default]
    Smart,
    Sensitive,
    Insensitive,
}

impl CaseMode {
    fn sensitive_for(self, needle: &str) -> bool {
        match self {
            CaseMode::Sensitive => true,
            CaseMode::Insensitive => false,
            CaseMode::Smart => needle.chars().any(char::is_uppercase),
        }
    }
}

/// Every occurrence of `needle`, left to right, non-overlapping.
///
/// An empty needle matches nothing: reporting a match at every position would be
/// technically defensible and useless.
pub fn find_all(text: &Rope, needle: &str, mode: CaseMode) -> Vec<Match> {
    if needle.is_empty() {
        return Vec::new();
    }
    let sensitive = mode.sensitive_for(needle);

    // Searching a `String` rather than walking the rope: find/replace runs on a keystroke,
    // not per frame, and a whole-buffer scan is simpler to get right than a chunk-aware
    // one. If large files ever make this hurt, ropey's chunk API is the escape hatch.
    let haystack = text.to_string();
    let (haystack, needle) = if sensitive {
        (haystack, needle.to_string())
    } else {
        (haystack.to_lowercase(), needle.to_lowercase())
    };

    // Byte offsets from `match_indices`, converted to chars — the unit everything else
    // in this crate speaks (ADR-0006 §1).
    let mut matches = Vec::new();
    let needle_chars = needle.chars().count();
    for (byte, _) in haystack.match_indices(&needle) {
        let start = haystack[..byte].chars().count();
        matches.push((start, start + needle_chars));
    }
    matches
}

/// The first match at or after `from`, wrapping to the start.
pub fn next_from(matches: &[Match], from: usize) -> Option<usize> {
    if matches.is_empty() {
        return None;
    }
    Some(matches.iter().position(|(start, _)| *start >= from).unwrap_or(0))
}

/// The last match starting strictly before `from`, wrapping to the end.
///
/// Callers navigating backwards pass the *current match's start*, not the raw cursor: a
/// cursor sitting inside a match would otherwise find that same match again and appear
/// stuck.
pub fn prev_from(matches: &[Match], from: usize) -> Option<usize> {
    if matches.is_empty() {
        return None;
    }
    Some(matches.iter().rposition(|(start, _)| *start < from).unwrap_or(matches.len() - 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rope(s: &str) -> Rope {
        Rope::from_str(s)
    }

    #[test]
    fn every_occurrence_is_found_in_order() {
        let text = rope("one two one three one");
        let found = find_all(&text, "one", CaseMode::Sensitive);
        assert_eq!(found, [(0, 3), (8, 11), (18, 21)]);
    }

    #[test]
    fn nothing_matches_an_empty_query() {
        // A match at every position is technically defensible and useless.
        assert!(find_all(&rope("anything"), "", CaseMode::Smart).is_empty());
    }

    #[test]
    fn a_missing_needle_finds_nothing() {
        assert!(find_all(&rope("abc"), "zzz", CaseMode::Smart).is_empty());
    }

    #[test]
    fn smart_case_is_insensitive_until_you_type_a_capital() {
        let text = rope("Error error ERROR");
        assert_eq!(find_all(&text, "error", CaseMode::Smart).len(), 3, "all three");
        assert_eq!(find_all(&text, "Error", CaseMode::Smart), [(0, 5)], "the capital narrows it");
    }

    #[test]
    fn explicit_modes_override_the_smart_default() {
        let text = rope("Error error");
        assert_eq!(find_all(&text, "Error", CaseMode::Insensitive).len(), 2);
        assert_eq!(find_all(&text, "error", CaseMode::Sensitive).len(), 1);
    }

    #[test]
    fn matches_are_char_offsets_not_byte_offsets() {
        // "héllo" is 5 chars, 6 bytes: a byte offset would land mid-character.
        let text = rope("héllo world héllo");
        assert_eq!(find_all(&text, "world", CaseMode::Sensitive), [(6, 11)]);
    }

    #[test]
    fn matches_span_lines_correctly() {
        let text = rope("first\nsecond\nfirst\n");
        assert_eq!(find_all(&text, "first", CaseMode::Sensitive), [(0, 5), (13, 18)]);
    }

    #[test]
    fn overlapping_candidates_are_reported_without_overlap() {
        // "aa" in "aaaa" is found at 0 and 2, not 0/1/2.
        assert_eq!(find_all(&rope("aaaa"), "aa", CaseMode::Sensitive), [(0, 2), (2, 4)]);
    }

    // --- navigation -----------------------------------------------------------------

    #[test]
    fn next_finds_the_match_at_or_after_the_cursor() {
        let matches = [(0, 3), (8, 11), (18, 21)];
        assert_eq!(next_from(&matches, 0), Some(0));
        assert_eq!(next_from(&matches, 1), Some(1));
        assert_eq!(next_from(&matches, 8), Some(1), "a cursor sitting on one stays on it");
        assert_eq!(next_from(&matches, 12), Some(2));
    }

    #[test]
    fn next_wraps_past_the_last_match() {
        let matches = [(0, 3), (8, 11)];
        assert_eq!(next_from(&matches, 99), Some(0), "back to the top");
    }

    #[test]
    fn prev_finds_the_last_match_starting_before_the_offset_and_wraps() {
        let matches = [(0, 3), (8, 11), (18, 21)];
        assert_eq!(prev_from(&matches, 18), Some(1));
        assert_eq!(prev_from(&matches, 8), Some(0));
        assert_eq!(prev_from(&matches, 0), Some(2), "wraps to the end");
    }

    /// Stepping backwards from inside a match must not land on that same match, which is
    /// why callers pass the current match's start rather than the cursor.
    #[test]
    fn stepping_backwards_repeatedly_walks_the_matches() {
        let matches = [(0, 3), (8, 11), (18, 21)];
        let mut at = 2; // sitting on the last match

        at = prev_from(&matches, matches[at].0).unwrap();
        assert_eq!(at, 1);
        at = prev_from(&matches, matches[at].0).unwrap();
        assert_eq!(at, 0);
        at = prev_from(&matches, matches[at].0).unwrap();
        assert_eq!(at, 2, "and wraps");
    }

    #[test]
    fn navigating_an_empty_result_set_goes_nowhere() {
        assert_eq!(next_from(&[], 0), None);
        assert_eq!(prev_from(&[], 0), None);
    }
}
