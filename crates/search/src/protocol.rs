use std::path::{Path, PathBuf};

use serde_json::Value;
use termesh_core::SearchMatch;

#[derive(Debug, thiserror::Error)]
pub enum SearchError {
    #[error("malformed ripgrep JSON: {0}")]
    MalformedJson(#[from] serde_json::Error),
    #[error("malformed ripgrep match record")]
    MalformedMatch,
    #[error("could not run search: {0}")]
    Process(String),
}

/// Parse one line from `rg --json`, ignoring non-match protocol records.
///
/// Returns one [`SearchMatch`] per *occurrence*, not per line. Ripgrep emits a single
/// `match` record for a line and lists every hit on it under `submatches`, so taking
/// only the first would make `rg` disagree with [`crate::literal_matches`] — which the
/// native fallback and the open-buffer overlay both use — about how many results the
/// same query has (ADR-0009 §1: the two paths are one user-facing contract).
pub fn parse_rg_json_line(root: &Path, raw: &str) -> Result<Vec<SearchMatch>, SearchError> {
    let value: Value = serde_json::from_str(raw)?;
    if value.get("type").and_then(Value::as_str) != Some("match") {
        return Ok(Vec::new());
    }

    let data = value.get("data").ok_or(SearchError::MalformedMatch)?;
    let raw_path = data
        .get("path")
        .and_then(|path| path.get("text"))
        .and_then(Value::as_str)
        .ok_or(SearchError::MalformedMatch)?;
    let line_text = data
        .get("lines")
        .and_then(|lines| lines.get("text"))
        .and_then(Value::as_str)
        .ok_or(SearchError::MalformedMatch)?;
    let line =
        data.get("line_number").and_then(Value::as_u64).ok_or(SearchError::MalformedMatch)?;
    let submatches =
        data.get("submatches").and_then(Value::as_array).ok_or(SearchError::MalformedMatch)?;
    if submatches.is_empty() {
        return Err(SearchError::MalformedMatch);
    }

    let line = usize::try_from(line).map_err(|_| SearchError::MalformedMatch)?;
    let path = PathBuf::from(raw_path);
    let path = if path.is_absolute() { path } else { root.join(path) };
    let text = line_text.trim_end_matches(['\r', '\n']).to_owned();

    submatches
        .iter()
        .map(|submatch| {
            let start =
                submatch.get("start").and_then(Value::as_u64).ok_or(SearchError::MalformedMatch)?;
            let start = usize::try_from(start).map_err(|_| SearchError::MalformedMatch)?;
            // Ripgrep reports byte offsets; the model and the editor both address
            // columns by character, so convert here rather than at every use site.
            let prefix = line_text.get(..start).ok_or(SearchError::MalformedMatch)?;
            Ok(SearchMatch {
                path: path.clone(),
                line: Some(line),
                column: Some(prefix.chars().count() + 1),
                text: Some(text.clone()),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_match_records_are_ignored() {
        assert!(parse_rg_json_line(Path::new("/repo"), r#"{"type":"begin","data":{}}"#)
            .unwrap()
            .is_empty());
    }

    /// Ripgrep reports a line once and lists its hits under `submatches`. Reporting only
    /// the first would give a different result count than the native fallback for the
    /// very same query.
    #[test]
    fn every_submatch_on_a_line_becomes_its_own_result() {
        let raw = r#"{"type":"match","data":{"path":{"text":"src/lib.rs"},"lines":{"text":"Alpha alpha\n"},"line_number":1,"submatches":[{"match":{"text":"Alpha"},"start":0,"end":5},{"match":{"text":"alpha"},"start":6,"end":11}]}}"#;
        let parsed = parse_rg_json_line(Path::new("/repo"), raw).unwrap();
        assert_eq!(parsed.iter().map(|found| found.column).collect::<Vec<_>>(), [Some(1), Some(7)]);
    }

    #[test]
    fn incomplete_match_is_rejected() {
        assert!(matches!(
            parse_rg_json_line(Path::new("/repo"), r#"{"type":"match","data":{}}"#),
            Err(SearchError::MalformedMatch)
        ));
    }
}
