use std::path::{Path, PathBuf};

use serde_json::Value;
use termesh_core::{Problem, ProblemSeverity};

use crate::{DecodedTaskOutput, TaskOutputDecoder};

pub struct CargoOutputDecoder {
    cwd: PathBuf,
    pending: Vec<u8>,
}

impl CargoOutputDecoder {
    pub fn new(cwd: PathBuf) -> Self {
        Self { cwd, pending: Vec::new() }
    }

    fn decode_line(&self, line: &[u8]) -> DecodedTaskOutput {
        let Ok(text) = std::str::from_utf8(line) else {
            return DecodedTaskOutput { display: line.to_vec(), problems: Vec::new() };
        };
        let json_text = text.strip_suffix('\n').unwrap_or(text);
        let json_text = json_text.strip_suffix('\r').unwrap_or(json_text);
        let Ok(value) = serde_json::from_str::<Value>(json_text) else {
            return fallback_line(&self.cwd, line);
        };
        match value.get("reason").and_then(Value::as_str) {
            Some("compiler-message") => compiler_message(&self.cwd, &value),
            Some("compiler-artifact" | "build-script-executed" | "build-finished") => {
                DecodedTaskOutput::default()
            }
            _ => DecodedTaskOutput { display: line.to_vec(), problems: Vec::new() },
        }
    }
}

impl TaskOutputDecoder for CargoOutputDecoder {
    fn push(&mut self, bytes: &[u8]) -> DecodedTaskOutput {
        self.pending.extend_from_slice(bytes);
        let mut decoded = DecodedTaskOutput::default();
        while let Some(end) = self.pending.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = self.pending.drain(..=end).collect();
            merge(&mut decoded, self.decode_line(&line));
        }
        decoded
    }

    fn finish(&mut self) -> DecodedTaskOutput {
        if self.pending.is_empty() {
            return DecodedTaskOutput::default();
        }
        let line = std::mem::take(&mut self.pending);
        self.decode_line(&line)
    }
}

fn compiler_message(cwd: &Path, value: &Value) -> DecodedTaskOutput {
    let Some(message) = value.get("message") else { return DecodedTaskOutput::default() };
    let display = message
        .get("rendered")
        .and_then(Value::as_str)
        .map(str::as_bytes)
        .unwrap_or_default()
        .to_vec();
    let problem = message
        .get("spans")
        .and_then(Value::as_array)
        .and_then(|spans| {
            spans.iter().find(|span| span.get("is_primary") == Some(&Value::Bool(true)))
        })
        .and_then(|span| problem_from_span(cwd, message, span));
    DecodedTaskOutput { display, problems: problem.into_iter().collect() }
}

fn problem_from_span(cwd: &Path, message: &Value, span: &Value) -> Option<Problem> {
    let raw_path = span.get("file_name")?.as_str()?;
    let path = PathBuf::from(raw_path);
    let path = if path.is_absolute() { path } else { cwd.join(path) };
    let level = message.get("level").and_then(Value::as_str).unwrap_or("warning");
    Some(Problem {
        path,
        line: usize::try_from(span.get("line_start")?.as_u64()?).ok()?,
        column: usize::try_from(span.get("column_start")?.as_u64()?).ok()?,
        severity: if level == "error" { ProblemSeverity::Error } else { ProblemSeverity::Warning },
        message: message.get("message")?.as_str()?.to_owned(),
    })
}

fn fallback_line(cwd: &Path, line: &[u8]) -> DecodedTaskOutput {
    let plain = strip_ansi(&String::from_utf8_lossy(line));
    let problems = panic_problem(cwd, &plain).into_iter().collect();
    DecodedTaskOutput { display: line.to_vec(), problems }
}

fn panic_problem(cwd: &Path, line: &str) -> Option<Problem> {
    let location = line.split("panicked at ").nth(1)?.split_whitespace().next()?;
    let location = location
        .trim_matches(|character| matches!(character, '\'' | '"' | ','))
        .trim_end_matches(':');
    let mut parts = location.rsplitn(3, ':');
    let column = parts.next()?.trim_end_matches(':').parse().ok()?;
    let line_number = parts.next()?.parse().ok()?;
    let raw_path = parts.next()?;
    let path = PathBuf::from(raw_path);
    Some(Problem {
        path: if path.is_absolute() { path } else { cwd.join(path) },
        line: line_number,
        column,
        severity: ProblemSeverity::Error,
        message: line.trim().to_owned(),
    })
}

fn strip_ansi(input: &str) -> String {
    let mut output = String::new();
    let mut chars = input.chars().peekable();
    while let Some(character) = chars.next() {
        if character == '\u{1b}' && chars.peek() == Some(&'[') {
            chars.next();
            for next in chars.by_ref() {
                if ('@'..='~').contains(&next) {
                    break;
                }
            }
        } else {
            output.push(character);
        }
    }
    output
}

fn merge(target: &mut DecodedTaskOutput, source: DecodedTaskOutput) {
    target.display.extend(source.display);
    target.problems.extend(source.problems);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compiler_json_split_across_chunks_becomes_rendered_output_and_problem() {
        let json = r#"{"reason":"compiler-message","message":{"rendered":"error[E0425]\n","level":"error","message":"cannot find value","spans":[{"file_name":"src/lib.rs","line_start":12,"column_start":5,"is_primary":true}]}}
"#;
        let mut decoder = CargoOutputDecoder::new(PathBuf::from("/p"));
        let split = json.len() / 2;
        assert!(decoder.push(&json.as_bytes()[..split]).display.is_empty());
        let output = decoder.push(&json.as_bytes()[split..]);
        assert_eq!(output.display, b"error[E0425]\n");
        assert_eq!((output.problems[0].line, output.problems[0].column), (12, 5));
    }

    #[test]
    fn artifacts_are_hidden_but_test_output_passes_through() {
        let mut decoder = CargoOutputDecoder::new(PathBuf::from("/p"));
        assert!(decoder.push(b"{\"reason\":\"compiler-artifact\"}\n").display.is_empty());
        assert_eq!(decoder.push(b"test result: ok\n").display, b"test result: ok\n");
    }

    #[test]
    fn ansi_panic_location_becomes_a_problem_without_changing_display() {
        let mut decoder = CargoOutputDecoder::new(PathBuf::from("/p"));
        let line = b"\x1b[31mthread 'case' panicked at src/lib.rs:12:5:\x1b[0m\n";
        let output = decoder.push(line);
        assert_eq!(output.display, line);
        assert_eq!(output.problems[0].path, Path::new("/p/src/lib.rs"));
        assert_eq!((output.problems[0].line, output.problems[0].column), (12, 5));
    }

    #[test]
    fn incomplete_non_json_line_is_flushed_verbatim() {
        let mut decoder = CargoOutputDecoder::new(PathBuf::from("/p"));
        assert!(decoder.push(b"partial").display.is_empty());
        assert_eq!(decoder.finish().display, b"partial");
    }
}
