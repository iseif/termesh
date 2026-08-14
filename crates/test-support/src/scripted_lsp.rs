use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use termesh_core::{LspEvent, LspRequest};
use termesh_lsp::LanguageService;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FakeLspCall {
    Send(LspRequest),
}

#[derive(Clone)]
pub struct FakeLspControl {
    calls: Arc<Mutex<Vec<FakeLspCall>>>,
}

impl FakeLspControl {
    pub fn calls(&self) -> Vec<FakeLspCall> {
        self.calls.lock().expect("fake LSP call log poisoned").clone()
    }
}

#[derive(Default)]
pub struct ScriptedLanguageServer {
    events: VecDeque<Vec<LspEvent>>,
    calls: Arc<Mutex<Vec<FakeLspCall>>>,
}

impl ScriptedLanguageServer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn control(&self) -> FakeLspControl {
        FakeLspControl { calls: self.calls.clone() }
    }

    pub fn with_events(mut self, events: Vec<LspEvent>) -> Self {
        self.events.push_back(events);
        self
    }
}

impl LanguageService for ScriptedLanguageServer {
    fn send(&mut self, request: LspRequest) {
        self.calls.lock().expect("fake LSP call log poisoned").push(FakeLspCall::Send(request));
    }

    fn poll(&mut self) -> Vec<LspEvent> {
        self.events.pop_front().unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use termesh_core::{LspEvent, LspRequest};
    use termesh_lsp::LanguageService;

    #[test]
    fn sends_are_recorded_in_order() {
        let mut server = ScriptedLanguageServer::new();
        let control = server.control();
        server.send(LspRequest::Shutdown);
        server.send(LspRequest::Shutdown);
        assert_eq!(
            control.calls(),
            vec![FakeLspCall::Send(LspRequest::Shutdown), FakeLspCall::Send(LspRequest::Shutdown)]
        );
    }

    #[test]
    fn poll_replays_one_batch_at_a_time_then_returns_empty() {
        let mut server = ScriptedLanguageServer::new()
            .with_events(vec![LspEvent::Started])
            .with_events(vec![LspEvent::Ready]);
        assert_eq!(server.poll(), vec![LspEvent::Started]);
        assert_eq!(server.poll(), vec![LspEvent::Ready]);
        assert!(server.poll().is_empty());
        assert!(server.poll().is_empty(), "an exhausted script is never a panic");
    }
}
