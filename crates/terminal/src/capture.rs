//! Bounded, UTF-8-safe output retained for ACP `terminal/output` (ADR-0008 §2).

pub const DEFAULT_CAPTURE_LIMIT: usize = 1_048_576;
pub const MAX_CAPTURE_LIMIT: usize = 8_388_608;

#[derive(Debug, Clone)]
pub struct CapturedOutput {
    text: String,
    pending_utf8: Vec<u8>,
    limit: usize,
    truncated: bool,
}

impl CapturedOutput {
    pub fn new(requested: usize) -> Self {
        Self {
            text: String::new(),
            pending_utf8: Vec::new(),
            limit: requested.min(MAX_CAPTURE_LIMIT),
            truncated: false,
        }
    }

    /// Append one PTY chunk. Incomplete UTF-8 at the chunk boundary is held until the
    /// next chunk so a valid stream is not corrupted merely because the reader split it.
    pub fn push(&mut self, bytes: &[u8]) {
        self.pending_utf8.extend_from_slice(bytes);
        self.decode_complete_prefix(false);
        self.enforce_limit();
    }

    /// Flush an incomplete final sequence as replacement text when the process exits.
    pub fn finish(&mut self) {
        self.decode_complete_prefix(true);
        self.enforce_limit();
    }

    pub fn as_str(&self) -> &str {
        &self.text
    }

    pub fn truncated(&self) -> bool {
        self.truncated
    }

    fn decode_complete_prefix(&mut self, finish: bool) {
        loop {
            match std::str::from_utf8(&self.pending_utf8) {
                Ok(valid) => {
                    self.text.push_str(valid);
                    self.pending_utf8.clear();
                    return;
                }
                Err(error) => {
                    let valid_up_to = error.valid_up_to();
                    if valid_up_to > 0 {
                        // SAFETY is not needed: `valid_up_to` is supplied by `from_utf8`.
                        let valid = std::str::from_utf8(&self.pending_utf8[..valid_up_to])
                            .expect("validated UTF-8 prefix");
                        self.text.push_str(valid);
                        self.pending_utf8.drain(..valid_up_to);
                    }

                    match error.error_len() {
                        Some(invalid_len) => {
                            self.text.push('\u{fffd}');
                            self.pending_utf8.drain(..invalid_len);
                        }
                        None if finish => {
                            self.text.push('\u{fffd}');
                            self.pending_utf8.clear();
                            return;
                        }
                        None => return,
                    }
                }
            }
        }
    }

    fn enforce_limit(&mut self) {
        if self.text.len() <= self.limit {
            return;
        }

        let mut remove = self.text.len() - self.limit;
        while remove < self.text.len() && !self.text.is_char_boundary(remove) {
            remove += 1;
        }
        self.text.drain(..remove);
        self.truncated = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_drops_oldest_text_on_a_utf8_boundary() {
        let mut out = CapturedOutput::new(4);
        out.push("éabc".as_bytes());
        assert_eq!(out.as_str(), "abc");
        assert!(out.truncated());
    }

    #[test]
    fn utf8_split_across_chunks_is_decoded_once_complete() {
        let mut out = CapturedOutput::new(DEFAULT_CAPTURE_LIMIT);
        out.push(&[0xc3]);
        assert_eq!(out.as_str(), "");
        out.push(&[0xa9]);
        assert_eq!(out.as_str(), "é");
        assert!(!out.truncated());
    }

    #[test]
    fn finishing_replaces_an_incomplete_utf8_sequence() {
        let mut out = CapturedOutput::new(DEFAULT_CAPTURE_LIMIT);
        out.push(&[0xc3]);
        out.finish();
        assert_eq!(out.as_str(), "�");
    }
}
