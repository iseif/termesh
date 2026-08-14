use termesh_platform::{ClipboardError, ClipboardService};

#[derive(Debug, Default)]
pub struct FakeClipboard {
    last_text: Option<String>,
}

impl FakeClipboard {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn last_text(&self) -> Option<&str> {
        self.last_text.as_deref()
    }
}

impl ClipboardService for FakeClipboard {
    fn set_text(&mut self, text: &str) -> Result<(), ClipboardError> {
        self.last_text = Some(text.to_owned());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use termesh_platform::ClipboardService;

    #[test]
    fn fake_records_the_latest_human_copy() {
        let mut clipboard = FakeClipboard::new();
        clipboard.set_text("first").unwrap();
        clipboard.set_text("second").unwrap();
        assert_eq!(clipboard.last_text(), Some("second"));
    }
}
