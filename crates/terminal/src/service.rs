//! Synchronous PTY service boundary. Calls run only on [`crate::PtyWorker`] (ADR-0008).

use std::sync::Arc;

use termesh_core::{PtyEvent, TerminalGeneration, TerminalId, TerminalSize, TerminalSpec};

pub type PtyEventSink = Arc<dyn Fn(PtyEvent) + Send + Sync>;
pub type PtyResult<T> = Result<T, PtyError>;

#[derive(Debug, thiserror::Error)]
pub enum PtyError {
    #[error("terminal already exists: {0}")]
    AlreadyExists(TerminalId),
    #[error("unknown terminal: {0}")]
    UnknownTerminal(TerminalId),
    #[error("PTY {operation} failed: {message}")]
    Backend { operation: &'static str, message: String },
}

impl PtyError {
    pub fn backend(operation: &'static str, error: impl std::fmt::Display) -> Self {
        Self::Backend { operation, message: error.to_string() }
    }
}

pub trait PtyService: Send + 'static {
    fn spawn(
        &mut self,
        terminal: TerminalId,
        generation: TerminalGeneration,
        spec: TerminalSpec,
        size: TerminalSize,
        sink: PtyEventSink,
    ) -> PtyResult<()>;

    fn write(&mut self, terminal: TerminalId, bytes: &[u8]) -> PtyResult<()>;

    fn resize(&mut self, terminal: TerminalId, size: TerminalSize) -> PtyResult<()>;

    fn kill(&mut self, terminal: TerminalId) -> PtyResult<()>;

    fn release(&mut self, terminal: TerminalId) -> PtyResult<()>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use termesh_core::TerminalId;

    #[test]
    fn unknown_terminal_errors_name_the_terminal() {
        let id = TerminalId::new(7);
        assert_eq!(PtyError::UnknownTerminal(id).to_string(), "unknown terminal: TerminalId(7)");
    }
}
