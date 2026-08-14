//! `Content-Length` framing for the language-server base protocol.
//!
//! Reads are arbitrary byte chunks: a header or UTF-8 code point may be split across
//! reads, and one read may contain several complete frames.

use serde_json::Value;

const HEADER_END: &[u8] = b"\r\n\r\n";

pub fn encode_frame(body: &Value) -> Vec<u8> {
    let body = serde_json::to_vec(body).expect("serialising a JSON value cannot fail");
    let mut frame = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    frame.extend_from_slice(&body);
    frame
}

#[derive(Debug, Default)]
pub struct FrameReader {
    buffer: Vec<u8>,
}

impl FrameReader {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, bytes: &[u8]) {
        self.buffer.extend_from_slice(bytes);
    }

    pub fn next_frame(&mut self) -> Option<Value> {
        loop {
            let header_end = self.buffer.windows(HEADER_END.len()).position(|w| w == HEADER_END)?;
            let body_start = header_end + HEADER_END.len();
            let content_length =
                std::str::from_utf8(&self.buffer[..header_end]).ok().and_then(|headers| {
                    headers.lines().find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())
                            .flatten()
                    })
                });
            let Some(content_length) = content_length else {
                self.buffer.drain(..body_start);
                continue;
            };
            let Some(frame_end) = body_start.checked_add(content_length) else {
                self.buffer.clear();
                return None;
            };
            if self.buffer.len() < frame_end {
                return None;
            }

            let body = serde_json::from_slice(&self.buffer[body_start..frame_end]).ok();
            self.buffer.drain(..frame_end);
            if body.is_some() {
                return body;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_frame_round_trips() {
        let body = serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize"});
        let bytes = encode_frame(&body);
        let text = String::from_utf8(bytes.clone()).unwrap();
        assert!(text.starts_with("Content-Length: "), "{text}");
        assert!(text.contains("\r\n\r\n"), "header terminator is CRLFCRLF");

        let mut reader = FrameReader::new();
        reader.push(&bytes);
        assert_eq!(reader.next_frame().unwrap(), body);
    }

    #[test]
    fn a_header_split_across_reads_is_reassembled() {
        let body = serde_json::json!({"jsonrpc":"2.0","method":"initialized"});
        let bytes = encode_frame(&body);
        let (head, tail) = bytes.split_at(8);

        let mut reader = FrameReader::new();
        reader.push(head);
        assert!(reader.next_frame().is_none(), "a partial header is not a frame");
        reader.push(tail);
        assert_eq!(reader.next_frame().unwrap(), body);
    }

    #[test]
    fn a_body_split_mid_utf8_is_reassembled() {
        let body = serde_json::json!({"message":"🦀 indexing"});
        let bytes = encode_frame(&body);
        let crab = "🦀".as_bytes();
        let crab_start = bytes.windows(crab.len()).position(|window| window == crab).unwrap();
        let split = crab_start + 2;
        let mut reader = FrameReader::new();
        reader.push(&bytes[..split]);
        assert!(reader.next_frame().is_none());
        reader.push(&bytes[split..]);
        assert_eq!(reader.next_frame().unwrap(), body);
    }

    #[test]
    fn two_frames_in_one_read_are_both_returned() {
        let a = serde_json::json!({"id":1});
        let b = serde_json::json!({"id":2});
        let mut bytes = encode_frame(&a);
        bytes.extend_from_slice(&encode_frame(&b));

        let mut reader = FrameReader::new();
        reader.push(&bytes);
        assert_eq!(reader.next_frame().unwrap(), a);
        assert_eq!(reader.next_frame().unwrap(), b);
        assert!(reader.next_frame().is_none());
    }

    #[test]
    fn header_names_are_case_insensitive_and_content_type_is_ignored() {
        let body = serde_json::json!({"id": 9});
        let encoded = serde_json::to_vec(&body).unwrap();
        let mut bytes = format!(
            "Content-Type: application/vscode-jsonrpc; charset=utf-8\r\ncontent-length: {}\r\n\r\n",
            encoded.len()
        )
        .into_bytes();
        bytes.extend_from_slice(&encoded);

        let mut reader = FrameReader::new();
        reader.push(&bytes);
        assert_eq!(reader.next_frame(), Some(body));
    }

    #[test]
    fn a_malformed_frame_does_not_strand_the_valid_frame_after_it() {
        let malformed = b"Content-Length: 1\r\n\r\n{";
        let body = serde_json::json!({"id":10});
        let mut bytes = malformed.to_vec();
        bytes.extend_from_slice(&encode_frame(&body));

        let mut reader = FrameReader::new();
        reader.push(&bytes);
        assert_eq!(reader.next_frame(), Some(body));
    }
}
