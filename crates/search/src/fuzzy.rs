use std::path::PathBuf;

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Score {
    filename_miss: bool,
    gap_count: usize,
    first_match: usize,
    path_len: usize,
    stable_path: String,
}

/// Filter and rank workspace-relative paths with deterministic subsequence matching.
///
/// Lowercase queries are case-insensitive; the presence of any uppercase character
/// switches to case-sensitive matching, mirroring ripgrep's smart-case behavior.
pub fn rank_files(files: &[PathBuf], query: &str) -> Vec<PathBuf> {
    if query.is_empty() {
        let mut sorted = files.to_vec();
        sorted.sort_by_key(|path| path.to_string_lossy().into_owned());
        return sorted;
    }

    let sensitive = query.chars().any(char::is_uppercase);
    let query: Vec<char> = query.chars().collect();
    let mut scored: Vec<(Score, PathBuf)> = files
        .iter()
        .filter_map(|path| {
            let display = path.to_string_lossy();
            let positions = subsequence_positions(&display, &query, sensitive)?;
            let filename_matches = path
                .file_name()
                .map(|name| {
                    subsequence_positions(&name.to_string_lossy(), &query, sensitive).is_some()
                })
                .unwrap_or(false);
            let gap_count =
                positions.windows(2).map(|pair| pair[1].saturating_sub(pair[0] + 1)).sum();
            Some((
                Score {
                    filename_miss: !filename_matches,
                    gap_count,
                    first_match: positions[0],
                    path_len: display.chars().count(),
                    stable_path: display.into_owned(),
                },
                path.clone(),
            ))
        })
        .collect();

    scored.sort_by(|left, right| left.0.cmp(&right.0));
    scored.into_iter().map(|(_, path)| path).collect()
}

fn subsequence_positions(haystack: &str, needle: &[char], sensitive: bool) -> Option<Vec<usize>> {
    let mut positions = Vec::with_capacity(needle.len());
    let mut wanted = needle.iter();
    let mut current = wanted.next()?;

    for (index, candidate) in haystack.chars().enumerate() {
        if chars_equal(candidate, *current, sensitive) {
            positions.push(index);
            match wanted.next() {
                Some(next) => current = next,
                None => return Some(positions),
            }
        }
    }
    None
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
    fn uppercase_query_is_case_sensitive() {
        let files = vec![PathBuf::from("alpha.rs"), PathBuf::from("Alpha.rs")];
        assert_eq!(rank_files(&files, "A"), vec![PathBuf::from("Alpha.rs")]);
    }

    #[test]
    fn empty_query_sorts_every_path() {
        let files = vec![PathBuf::from("z"), PathBuf::from("a")];
        assert_eq!(rank_files(&files, ""), vec![PathBuf::from("a"), PathBuf::from("z")]);
    }
}
