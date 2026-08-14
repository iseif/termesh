//! JSON-RPC 2.0 framing for the ACP transport (ADR-0007 §1, §2).
//!
//! ADR-0007 takes the *wire types* from `agent-client-protocol-schema` — that is where
//! protocol churn lives — and owns the framing, which has not changed since JSON-RPC 2.0
//! in 2010. This is that framing: one JSON object per line, in both directions.
//!
//! **Verified, not assumed.** The upstream SDK reads with `.lines()`, writes with
//! `write_line`, and asserts outgoing messages contain no `\r` or `\n`. So a line is a
//! message, and a message never spans lines.
//!
//! Pure: bytes in, typed messages out. No process, no threads, no I/O — which is what
//! makes the wire behaviour testable without an agent installed.

use serde_json::{json, Value};

/// A message we send to the agent, or one it sends us.
#[derive(Debug, Clone, PartialEq)]
pub enum Message {
    /// A call expecting a response, correlated by `id`.
    Request { id: u64, method: String, params: Value },
    /// A successful answer to a request we made.
    Response { id: u64, result: Value },
    /// A failed answer to a request we made.
    Error { id: u64, code: i64, message: String },
    /// A one-way message. `session/update` — the stream that carries everything
    /// interesting — is a notification.
    Notification { method: String, params: Value },
}

/// Why a line could not be understood.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    /// Not JSON at all. Agents write diagnostics to stdout more often than they should,
    /// so this is expected traffic rather than a fatal condition — the caller logs it and
    /// keeps reading.
    NotJson(String),
    /// JSON, but not a JSON-RPC message we recognise.
    Malformed(String),
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecodeError::NotJson(line) => write!(f, "not JSON: {}", truncate(line)),
            DecodeError::Malformed(line) => write!(f, "not a JSON-RPC message: {}", truncate(line)),
        }
    }
}

impl std::error::Error for DecodeError {}

fn truncate(s: &str) -> String {
    const MAX: usize = 120;
    if s.chars().count() <= MAX {
        return s.to_string();
    }
    let head: String = s.chars().take(MAX).collect();
    format!("{head}…")
}

impl Message {
    /// Serialise to a single line, newline included.
    pub fn encode(&self) -> String {
        let value = match self {
            Message::Request { id, method, params } => {
                json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
            }
            Message::Response { id, result } => {
                json!({ "jsonrpc": "2.0", "id": id, "result": result })
            }
            Message::Error { id, code, message } => {
                json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
            }
            Message::Notification { method, params } => {
                json!({ "jsonrpc": "2.0", "method": method, "params": params })
            }
        };
        // `to_string` never emits a newline, which is exactly the invariant the framing
        // depends on — a message must not span lines.
        format!("{value}\n")
    }

    /// Parse one line.
    pub fn decode(line: &str) -> Result<Message, DecodeError> {
        let line = line.trim();
        if line.is_empty() {
            return Err(DecodeError::NotJson(String::new()));
        }
        let value: Value =
            serde_json::from_str(line).map_err(|_| DecodeError::NotJson(line.to_string()))?;

        let id = value.get("id").and_then(Value::as_u64);
        let method = value.get("method").and_then(Value::as_str).map(str::to_string);
        let params = value.get("params").cloned().unwrap_or(Value::Null);

        match (id, method) {
            // A method with an id is a call from the agent — `fs/read_text_file` and
            // `session/request_permission` both arrive this way, and both need an answer.
            (Some(id), Some(method)) => Ok(Message::Request { id, method, params }),
            (None, Some(method)) => Ok(Message::Notification { method, params }),
            (Some(id), None) => {
                if let Some(error) = value.get("error") {
                    return Ok(Message::Error {
                        id,
                        code: error.get("code").and_then(Value::as_i64).unwrap_or(0),
                        message: error
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown error")
                            .to_string(),
                    });
                }
                Ok(Message::Response {
                    id,
                    result: value.get("result").cloned().unwrap_or(Value::Null),
                })
            }
            (None, None) => Err(DecodeError::Malformed(line.to_string())),
        }
    }
}

/// Hands out request ids.
///
/// Ids must be unique for the life of the connection: reusing one would let a late
/// response be matched to the wrong call.
#[derive(Debug, Default)]
pub struct RequestIds(u64);

impl RequestIds {
    /// Deliberately not named `next`: this is not an iterator, and a `RequestIds` that
    /// looked like one would invite `.collect()` on an infinite sequence.
    pub fn allocate(&mut self) -> u64 {
        self.0 += 1;
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(message: Message) {
        let line = message.encode();
        assert!(line.ends_with('\n'), "every message is one line");
        assert_eq!(line.matches('\n').count(), 1, "and only one");
        assert_eq!(Message::decode(&line), Ok(message));
    }

    #[test]
    fn every_message_kind_round_trips() {
        round_trip(Message::Request {
            id: 1,
            method: "session/new".into(),
            params: json!({ "cwd": "/proj" }),
        });
        round_trip(Message::Response { id: 1, result: json!({ "sessionId": "s1" }) });
        round_trip(Message::Error { id: 2, code: -32601, message: "no such method".into() });
        round_trip(Message::Notification {
            method: "session/update".into(),
            params: json!({ "update": { "sessionUpdate": "agent_message_chunk" } }),
        });
    }

    #[test]
    fn a_method_with_an_id_is_a_request_we_must_answer() {
        // `fs/read_text_file` arrives this way — the agent is asking *us* (ADR-0007 §3).
        let line =
            r#"{"jsonrpc":"2.0","id":7,"method":"fs/read_text_file","params":{"path":"/a"}}"#;
        match Message::decode(line).unwrap() {
            Message::Request { id, method, .. } => {
                assert_eq!((id, method.as_str()), (7, "fs/read_text_file"));
            }
            other => panic!("expected a request, got {other:?}"),
        }
    }

    #[test]
    fn a_method_without_an_id_is_a_notification() {
        let line = r#"{"jsonrpc":"2.0","method":"session/update","params":{}}"#;
        assert!(matches!(Message::decode(line), Ok(Message::Notification { .. })));
    }

    #[test]
    fn an_error_response_is_not_mistaken_for_a_result() {
        let line = r#"{"jsonrpc":"2.0","id":3,"error":{"code":-32000,"message":"boom"}}"#;
        assert_eq!(
            Message::decode(line),
            Ok(Message::Error { id: 3, code: -32000, message: "boom".into() })
        );
    }

    #[test]
    fn a_missing_error_message_still_decodes() {
        let line = r#"{"jsonrpc":"2.0","id":3,"error":{"code":-32000}}"#;
        assert!(matches!(Message::decode(line), Ok(Message::Error { .. })));
    }

    /// Agents log to stdout more often than they should. A stray line is traffic to skip,
    /// not a reason to tear down the session.
    #[test]
    fn noise_on_the_wire_is_reported_rather_than_fatal() {
        for line in ["Listening on stdio...", "", "   ", "warning: something"] {
            assert!(matches!(Message::decode(line), Err(DecodeError::NotJson(_))), "{line:?}");
        }
    }

    #[test]
    fn valid_json_that_is_not_jsonrpc_is_distinguished_from_noise() {
        assert!(matches!(Message::decode("{\"hello\":1}"), Err(DecodeError::Malformed(_))));
        assert!(matches!(Message::decode("[1,2,3]"), Err(DecodeError::Malformed(_))));
    }

    #[test]
    fn a_long_bad_line_is_truncated_in_the_error() {
        let noise = "x".repeat(5_000);
        let message = Message::decode(&noise).unwrap_err().to_string();
        assert!(message.len() < 200, "errors must stay readable, got {} chars", message.len());
        assert!(message.ends_with('…'));
    }

    #[test]
    fn surrounding_whitespace_is_tolerated() {
        let line = "  {\"jsonrpc\":\"2.0\",\"method\":\"ping\",\"params\":null}  \n";
        assert!(matches!(Message::decode(line), Ok(Message::Notification { .. })));
    }

    #[test]
    fn embedded_newlines_are_escaped_so_a_message_stays_one_line() {
        // The invariant the whole framing rests on: content with newlines must not split
        // a message in two.
        let message = Message::Notification {
            method: "session/update".into(),
            params: json!({ "text": "line one\nline two" }),
        };
        let encoded = message.encode();
        assert_eq!(encoded.matches('\n').count(), 1, "only the terminator");
        assert_eq!(Message::decode(&encoded), Ok(message));
    }

    #[test]
    fn request_ids_are_never_reused() {
        let mut ids = RequestIds::default();
        let issued: Vec<u64> = (0..100).map(|_| ids.allocate()).collect();
        let mut sorted = issued.clone();
        sorted.dedup();
        assert_eq!(issued.len(), sorted.len(), "a reused id could mismatch a late response");
        assert_eq!(issued[0], 1, "ids start at 1, so 0 stays available as 'no id'");
    }

    #[test]
    fn params_default_to_null_rather_than_failing() {
        let line = r#"{"jsonrpc":"2.0","method":"session/cancel"}"#;
        assert_eq!(
            Message::decode(line),
            Ok(Message::Notification { method: "session/cancel".into(), params: Value::Null })
        );
    }
}
