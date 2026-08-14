//! Human-only clipboard output through OSC 52 (ADR-0008 §3).

use std::io::{self, Write};

use base64::Engine;

pub const MAX_CLIPBOARD_BYTES: usize = 1_048_576;

#[derive(Debug, thiserror::Error)]
pub enum ClipboardError {
    #[error("clipboard text is {bytes} bytes; the limit is {max}")]
    TooLarge { bytes: usize, max: usize },
    #[error("could not write clipboard sequence: {0}")]
    Io(#[from] io::Error),
}

/// Widgets and the agent never touch the system clipboard directly.
pub trait ClipboardService: Send {
    fn set_text(&mut self, text: &str) -> Result<(), ClipboardError>;
}

/// A terminal-native clipboard writer which works locally and through SSH-aware hosts.
pub struct Osc52Clipboard<W> {
    writer: W,
}

impl<W> Osc52Clipboard<W> {
    pub fn new(writer: W) -> Self {
        Self { writer }
    }

    pub fn into_inner(self) -> W {
        self.writer
    }
}

impl<W: Write + Send> ClipboardService for Osc52Clipboard<W> {
    fn set_text(&mut self, text: &str) -> Result<(), ClipboardError> {
        if text.len() > MAX_CLIPBOARD_BYTES {
            return Err(ClipboardError::TooLarge { bytes: text.len(), max: MAX_CLIPBOARD_BYTES });
        }
        let encoded = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
        self.writer.write_all(b"\x1b]52;c;")?;
        self.writer.write_all(encoded.as_bytes())?;
        self.writer.write_all(b"\x07")?;
        self.writer.flush()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn osc52_encodes_text_and_flushes_one_complete_sequence() {
        let mut clipboard = Osc52Clipboard::new(Vec::new());
        clipboard.set_text("hello").unwrap();
        assert_eq!(clipboard.into_inner(), b"\x1b]52;c;aGVsbG8=\x07");
    }

    #[test]
    fn oversized_text_is_rejected_before_writing() {
        let mut clipboard = Osc52Clipboard::new(Vec::new());
        let text = "x".repeat(MAX_CLIPBOARD_BYTES + 1);
        assert!(matches!(clipboard.set_text(&text), Err(ClipboardError::TooLarge { .. })));
        assert!(clipboard.into_inner().is_empty());
    }
}
