//! Typed JSON-RPC 2.0 messages carried inside language-server frames.

use serde_json::{json, Value};

#[derive(Debug, Clone, PartialEq)]
pub enum Message {
    Request { id: u64, method: String, params: Value },
    Response { id: u64, result: Value },
    Error { id: u64, code: i64, message: String },
    Notification { method: String, params: Value },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    Malformed(String),
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecodeError::Malformed(value) => write!(f, "not a JSON-RPC message: {value}"),
        }
    }
}

impl std::error::Error for DecodeError {}

impl Message {
    pub fn decode(value: Value) -> Result<Message, DecodeError> {
        let id = value.get("id").and_then(Value::as_u64);
        let method = value.get("method").and_then(Value::as_str).map(str::to_string);
        let params = value.get("params").cloned().unwrap_or(Value::Null);

        match (id, method) {
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
            (None, None) => Err(DecodeError::Malformed(value.to_string())),
        }
    }

    pub fn encode(&self) -> Value {
        match self {
            Message::Request { id, method, params } => {
                json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})
            }
            Message::Response { id, result } => {
                json!({"jsonrpc":"2.0","id":id,"result":result})
            }
            Message::Error { id, code, message } => {
                json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}})
            }
            Message::Notification { method, params } => {
                json!({"jsonrpc":"2.0","method":method,"params":params})
            }
        }
    }
}

#[derive(Debug, Default)]
pub struct RequestIds(u64);

impl RequestIds {
    pub fn allocate(&mut self) -> u64 {
        self.0 += 1;
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn round_trip(message: Message) {
        assert_eq!(Message::decode(message.encode()), Ok(message));
    }

    #[test]
    fn every_message_kind_round_trips() {
        round_trip(Message::Request {
            id: 1,
            method: "initialize".into(),
            params: json!({"rootUri": "file:///proj"}),
        });
        round_trip(Message::Response { id: 1, result: json!({"capabilities": {}}) });
        round_trip(Message::Error { id: 2, code: -32601, message: "no such method".into() });
        round_trip(Message::Notification {
            method: "textDocument/publishDiagnostics".into(),
            params: json!({"diagnostics": []}),
        });
    }

    #[test]
    fn method_presence_and_id_distinguish_requests_from_notifications() {
        assert!(matches!(
            Message::decode(json!({"jsonrpc":"2.0","id":7,"method":"workspace/configuration"})),
            Ok(Message::Request { id: 7, .. })
        ));
        assert!(matches!(
            Message::decode(json!({"jsonrpc":"2.0","method":"window/logMessage"})),
            Ok(Message::Notification { .. })
        ));
    }

    #[test]
    fn error_responses_are_not_mistaken_for_results() {
        assert_eq!(
            Message::decode(json!({
                "jsonrpc":"2.0",
                "id":3,
                "error":{"code":-32000,"message":"boom"}
            })),
            Ok(Message::Error { id: 3, code: -32000, message: "boom".into() })
        );
    }

    #[test]
    fn request_ids_are_never_reused() {
        let mut ids = RequestIds::default();
        let issued: Vec<u64> = (0..100).map(|_| ids.allocate()).collect();
        assert_eq!(issued[0], 1);
        assert!(issued.windows(2).all(|pair| pair[0] < pair[1]));
    }
}
