use std::path::PathBuf;

use termesh_core::{Problem, ProblemSeverity};

use crate::{DecodedTaskOutput, TaskOutputDecoder};

pub struct TextProblemDecoder {
    cwd: PathBuf,
    pending: Vec<u8>,
}

impl TextProblemDecoder {
    pub fn new(cwd: PathBuf) -> Self {
        Self { cwd, pending: Vec::new() }
    }
}

impl TaskOutputDecoder for TextProblemDecoder {
    fn push(&mut self, bytes: &[u8]) -> DecodedTaskOutput {
        self.pending.extend_from_slice(bytes);
        let mut problems = Vec::new();
        while let Some(end) = self.pending.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = self.pending.drain(..=end).collect();
            if let Some(problem) = decode_line(&self.cwd, &line) {
                problems.push(problem);
            }
        }
        DecodedTaskOutput { display: bytes.to_vec(), problems }
    }

    fn finish(&mut self) -> DecodedTaskOutput {
        let line = std::mem::take(&mut self.pending);
        let problems = decode_line(&self.cwd, &line).into_iter().collect();
        DecodedTaskOutput { display: Vec::new(), problems }
    }
}

fn decode_line(cwd: &std::path::Path, line: &[u8]) -> Option<Problem> {
    let text = std::str::from_utf8(line).ok()?.trim_end_matches(['\r', '\n']);
    let (text, prefixed_severity) = strip_maven_prefix(text);
    let mut problem = tsc_problem(cwd, text)
        .or_else(|| gcc_problem(cwd, text))
        .or_else(|| javac_problem(cwd, text))
        .or_else(|| python_problem(cwd, text))?;
    if let Some(severity) = prefixed_severity {
        problem.severity = severity;
    }
    Some(problem)
}

fn strip_maven_prefix(text: &str) -> (&str, Option<ProblemSeverity>) {
    if let Some(text) = text.strip_prefix("[ERROR]") {
        (text.trim_start(), Some(ProblemSeverity::Error))
    } else if let Some(text) = text.strip_prefix("[WARNING]") {
        (text.trim_start(), Some(ProblemSeverity::Warning))
    } else {
        (text, None)
    }
}

fn tsc_problem(cwd: &std::path::Path, text: &str) -> Option<Problem> {
    let coordinates_end = text.rfind("):")?;
    let coordinates_start = text[..coordinates_end].rfind('(')?;
    let mut coordinates = text[coordinates_start + 1..coordinates_end].split(',');
    let line = coordinates.next()?.trim().parse().ok()?;
    let column = coordinates.next()?.trim().parse().ok()?;
    if coordinates.next().is_some() {
        return None;
    }
    build_problem(cwd, &text[..coordinates_start], line, column, text[coordinates_end + 2..].trim())
}

fn gcc_problem(cwd: &std::path::Path, text: &str) -> Option<Problem> {
    for (line_separator, _) in text.match_indices(':') {
        let after_line = &text[line_separator + 1..];
        let Some(column_separator) = after_line.find(':') else { continue };
        let Ok(line) = after_line[..column_separator].trim().parse() else { continue };
        let after_column = &after_line[column_separator + 1..];
        let Some(message_separator) = after_column.find(':') else { continue };
        let Ok(column) = after_column[..message_separator].trim().parse() else { continue };
        return build_problem(
            cwd,
            text[..line_separator].trim(),
            line,
            column,
            after_column[message_separator + 1..].trim(),
        );
    }
    None
}

fn javac_problem(cwd: &std::path::Path, text: &str) -> Option<Problem> {
    for (line_separator, _) in text.match_indices(':') {
        let after_line = &text[line_separator + 1..];
        let Some(message_separator) = after_line.find(':') else { continue };
        let Ok(line) = after_line[..message_separator].trim().parse() else { continue };
        let raw_path = text[..line_separator].trim();
        if !plausible_two_part_path(raw_path) {
            continue;
        }
        return build_problem(cwd, raw_path, line, 1, after_line[message_separator + 1..].trim());
    }
    None
}

fn plausible_two_part_path(raw_path: &str) -> bool {
    !raw_path.is_empty()
        && std::path::Path::new(raw_path).extension().is_some()
        && (raw_path.contains('/') || raw_path.contains('\\') || raw_path.contains('.'))
}

fn python_problem(cwd: &std::path::Path, text: &str) -> Option<Problem> {
    let frame = text.trim_start().strip_prefix("File \"")?;
    let marker = "\", line ";
    let path_end = frame.find(marker)?;
    let tail = &frame[path_end + marker.len()..];
    let line_text = tail.split([',', ' ']).next()?;
    let line = line_text.parse().ok()?;
    build_problem(cwd, &frame[..path_end], line, 1, text.trim())
}

fn build_problem(
    cwd: &std::path::Path,
    raw_path: &str,
    line: usize,
    column: usize,
    message: &str,
) -> Option<Problem> {
    use std::path::Component;

    if raw_path.is_empty() || message.is_empty() {
        return None;
    }
    let path = PathBuf::from(raw_path);
    if path.components().any(|component| component == Component::ParentDir) {
        return None;
    }
    let path = if path.is_absolute() { path } else { cwd.join(path) };
    // The leading token, not the whole message: `error TS2531: ... possibly a warning
    // about ...` is an error, and searching the body for the word would demote it.
    let severity = if message
        .split(|c: char| !c.is_ascii_alphabetic())
        .find(|word| !word.is_empty())
        .is_some_and(|word| {
            word.eq_ignore_ascii_case("warning") || word.eq_ignore_ascii_case("warn")
        }) {
        ProblemSeverity::Warning
    } else {
        ProblemSeverity::Error
    };
    Some(Problem { path, line, column, severity, message: message.to_string() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use termesh_core::ProblemSeverity;

    #[test]
    fn it_matches_the_tsc_shape() {
        let mut decoder = TextProblemDecoder::new("/p".into());
        let out = decoder.push(b"src/app.ts(12,5): error TS2304: Cannot find name 'foo'\n");
        let problem = &out.problems[0];
        assert_eq!(problem.path, Path::new("/p/src/app.ts"));
        assert_eq!((problem.line, problem.column), (12, 5));
        assert_eq!(problem.severity, ProblemSeverity::Error);
        assert!(problem.message.contains("Cannot find name"));
    }

    #[test]
    fn an_error_mentioning_the_word_warning_stays_an_error() {
        // Severity comes from the leading token. Searching the whole message would
        // demote any diagnostic whose text happens to discuss a warning.
        let mut decoder = TextProblemDecoder::new("/p".into());
        let out =
            decoder.push(b"src/app.ts(3,1): error TS2531: suppress this with a warning comment\n");
        assert_eq!(out.problems[0].severity, ProblemSeverity::Error);
    }

    #[test]
    fn it_matches_the_gcc_shape() {
        let mut decoder = TextProblemDecoder::new("/p".into());
        let out = decoder.push(b"src/app.js:12:5: warning: unused variable\n");
        assert_eq!(out.problems[0].severity, ProblemSeverity::Warning);
    }

    #[test]
    fn it_matches_the_javac_shape() {
        // javac emits a line but no column, so neither existing shape matches and Java
        // build failures currently produce no problems at all.
        let mut decoder = TextProblemDecoder::new("/p".into());
        let out =
            decoder.push(b"src/main/java/com/example/App.java:12: error: cannot find symbol\n");
        let problem = &out.problems[0];
        assert_eq!(problem.path, Path::new("/p/src/main/java/com/example/App.java"));
        assert_eq!(problem.line, 12);
        assert_eq!(problem.column, 1, "no column in the output; default to the line start");
        assert_eq!(problem.severity, ProblemSeverity::Error);
        assert!(problem.message.contains("cannot find symbol"));
    }

    #[test]
    fn a_three_part_line_still_matches_as_three_parts() {
        // Ordering matters: try the column shape first, or `app.js:12:5: broken`
        // becomes line 12 with the message "5: broken".
        let mut decoder = TextProblemDecoder::new("/p".into());
        let out = decoder.push(b"src/app.js:12:5: error: broken\n");
        assert_eq!((out.problems[0].line, out.problems[0].column), (12, 5));
    }

    #[test]
    fn a_maven_prefixed_javac_line_still_resolves_its_path() {
        let mut decoder = TextProblemDecoder::new("/p".into());
        let out = decoder.push(b"[ERROR] src/main/java/App.java:7: error: ';' expected\n");
        assert_eq!(out.problems[0].path, Path::new("/p/src/main/java/App.java"));
        assert_eq!(out.problems[0].line, 7);
    }

    #[test]
    fn a_maven_warning_prefix_is_authoritative() {
        let mut decoder = TextProblemDecoder::new("/p".into());
        let out = decoder.push(b"[WARNING] src/main/java/App.java:9: error: deprecated API\n");
        assert_eq!(out.problems[0].severity, ProblemSeverity::Warning);
    }

    #[test]
    fn a_javac_warning_is_still_a_warning() {
        let mut decoder = TextProblemDecoder::new("/p".into());
        let out = decoder.push(b"src/main/java/App.java:9: warning: deprecated API\n");
        assert_eq!(out.problems[0].severity, ProblemSeverity::Warning);
    }

    #[test]
    fn a_bare_timestamp_is_not_mistaken_for_a_location() {
        let mut decoder = TextProblemDecoder::new("/p".into());
        let out = decoder.push(b"12:30: build still running\n");
        assert!(out.problems.is_empty());
    }

    #[test]
    fn it_matches_a_python_traceback_frame() {
        let mut decoder = TextProblemDecoder::new("/p".into());
        let out = decoder.push(b"  File \"app.py\", line 12, in handler\n");
        assert_eq!(out.problems[0].line, 12);
    }

    #[test]
    fn unrecognised_output_reaches_the_display_untouched() {
        let mut decoder = TextProblemDecoder::new("/p".into());
        let line = b"\x1b[32m  vite v5 ready in 300ms\x1b[0m\n";
        let out = decoder.push(line);
        assert!(out.problems.is_empty());
        assert_eq!(out.display, line);
    }

    #[test]
    fn a_line_split_across_reads_is_matched_once_it_completes() {
        let mut decoder = TextProblemDecoder::new("/p".into());
        let first = decoder.push(b"src/app.js:12:");
        assert!(first.problems.is_empty());
        assert_eq!(first.display, b"src/app.js:12:");

        let second = decoder.push(b"5: error: broken\n");
        assert_eq!(second.problems.len(), 1);
        assert_eq!(second.display, b"5: error: broken\n");
    }

    #[test]
    fn absolute_paths_are_kept_and_traversal_is_refused() {
        let mut decoder = TextProblemDecoder::new("/p".into());
        let absolute = decoder.push(b"/outside/app.js:2:3: error: broken\n");
        assert_eq!(absolute.problems[0].path, Path::new("/outside/app.js"));

        let traversal = decoder.push(b"../secret.js:2:3: error: hidden\n");
        assert!(traversal.problems.is_empty());
    }
}
