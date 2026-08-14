use termesh_core::{LspEvent, LspRequest};

/// The language boundary. Long-lived and bidirectional: the server sends
/// diagnostics and progress unprompted, so this is the `AgentService` shape
/// (send/poll), not the `GitService` request/response shape (ADR-0011 §1).
pub trait LanguageService: Send {
    /// Queue work for the server. Never blocks the caller.
    fn send(&mut self, request: LspRequest);
    /// Take whatever the server has produced since the last call.
    fn poll(&mut self) -> Vec<LspEvent>;
}

#[derive(Debug, Default)]
pub struct NullLanguageService;

impl LanguageService for NullLanguageService {
    fn send(&mut self, _request: LspRequest) {}

    fn poll(&mut self) -> Vec<LspEvent> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_service_accepts_unscripted_work_without_panicking() {
        let mut service = NullLanguageService;
        service.send(LspRequest::Shutdown);
        assert!(service.poll().is_empty());
    }
}
