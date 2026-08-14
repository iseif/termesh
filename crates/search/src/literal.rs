use std::path::Path;

use termesh_core::SearchMatch;

/// Search an in-memory buffer without converting character columns to byte offsets.
pub fn literal_matches(path: &Path, contents: &str, query: &str) -> Vec<SearchMatch> {
    if query.is_empty() {
        return Vec::new();
    }

    let sensitive = query.chars().any(char::is_uppercase);
    let needle: Vec<char> = query.chars().collect();
    let mut matches = Vec::new();

    for (line_index, raw_line) in contents.split_inclusive('\n').enumerate() {
        let line = raw_line.strip_suffix('\n').unwrap_or(raw_line);
        let line = line.strip_suffix('\r').unwrap_or(line);
        let chars: Vec<char> = line.chars().collect();
        if needle.len() > chars.len() {
            continue;
        }
        // Leftmost-first and *non-overlapping*, so a hit consumes its own characters:
        // "aa" in "aaaa" is two results, not three. This is what `rg --fixed-strings`
        // reports, and what `termesh_editor::search` already does for in-buffer find —
        // matching both is what keeps one result list from mixing two conventions.
        let mut start = 0;
        while start + needle.len() <= chars.len() {
            let hit = chars[start..start + needle.len()]
                .iter()
                .zip(&needle)
                .all(|(left, right)| chars_equal(*left, *right, sensitive));
            if hit {
                matches.push(SearchMatch {
                    path: path.to_path_buf(),
                    line: Some(line_index + 1),
                    column: Some(start + 1),
                    text: Some(line.to_owned()),
                });
                start += needle.len();
            } else {
                start += 1;
            }
        }
    }
    matches
}

fn chars_equal(left: char, right: char, sensitive: bool) -> bool {
    if sensitive {
        left == right
    } else {
        left.to_lowercase().eq(right.to_lowercase())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unicode_columns_count_characters() {
        let found = literal_matches(Path::new("unicode.rs"), "éneedle", "needle");
        assert_eq!(found[0].column, Some(2));
    }

    #[test]
    fn empty_query_has_no_matches() {
        assert!(literal_matches(Path::new("a"), "anything", "").is_empty());
    }

    /// Verified against `rg --json -F -- aa` on "aaaa": submatches at 0 and 2, not
    /// 0, 1 and 2. Overlapping hits are common in real code — two spaces against an
    /// indent, `--` in a comment ruler — so this is not a corner case.
    #[test]
    fn occurrences_do_not_overlap() {
        let found = literal_matches(Path::new("a.rs"), "aaaa", "aa");
        assert_eq!(found.iter().map(|m| m.column).collect::<Vec<_>>(), [Some(1), Some(3)]);
    }
}
