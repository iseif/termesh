//! Deterministic PTY service for model and integration tests.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use termesh_core::{
    PtyEvent, PtyRequest, TerminalGeneration, TerminalId, TerminalSize, TerminalSpec,
};
use termesh_terminal::{PtyError, PtyEventSink, PtyResult, PtyService};

#[derive(Default)]
struct State {
    live: BTreeMap<TerminalId, (TerminalGeneration, PtyEventSink)>,
    history: Vec<PtyRequest>,
    pending: VecDeque<PtyRequest>,
}

type Shared = Arc<(Mutex<State>, Condvar)>;

#[derive(Clone, Default)]
pub struct ScriptedPty {
    shared: Shared,
}

#[derive(Clone)]
pub struct ScriptedPtyControl {
    shared: Shared,
}

impl ScriptedPty {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn control(&self) -> ScriptedPtyControl {
        ScriptedPtyControl { shared: self.shared.clone() }
    }

    fn record(&self, request: PtyRequest) {
        let (lock, ready) = &*self.shared;
        let mut state = lock.lock().expect("scripted PTY state poisoned");
        state.history.push(request.clone());
        state.pending.push_back(request);
        ready.notify_all();
    }
}

impl ScriptedPtyControl {
    pub fn requests(&self) -> Vec<PtyRequest> {
        self.shared.0.lock().expect("scripted PTY state poisoned").history.clone()
    }

    pub fn recv_request(&self, timeout: Duration) -> Option<PtyRequest> {
        let (lock, ready) = &*self.shared;
        let state = lock.lock().expect("scripted PTY state poisoned");
        let (mut state, _) = ready
            .wait_timeout_while(state, timeout, |state| state.pending.is_empty())
            .expect("scripted PTY state poisoned");
        state.pending.pop_front()
    }

    pub fn emit(&self, event: PtyEvent) -> bool {
        let terminal = event_terminal(&event);
        let sink = self
            .shared
            .0
            .lock()
            .expect("scripted PTY state poisoned")
            .live
            .get(&terminal)
            .map(|(_, sink)| sink.clone());
        if let Some(sink) = sink {
            sink(event);
            true
        } else {
            false
        }
    }
}

impl PtyService for ScriptedPty {
    fn spawn(
        &mut self,
        terminal: TerminalId,
        generation: TerminalGeneration,
        spec: TerminalSpec,
        size: TerminalSize,
        sink: PtyEventSink,
    ) -> PtyResult<()> {
        {
            let mut state = self.shared.0.lock().expect("scripted PTY state poisoned");
            if state.live.contains_key(&terminal) {
                return Err(PtyError::AlreadyExists(terminal));
            }
            state.live.insert(terminal, (generation, sink.clone()));
        }
        self.record(PtyRequest::Spawn { terminal, generation, spec, size });
        sink(PtyEvent::Spawned { terminal, generation, process_id: None });
        Ok(())
    }

    fn write(&mut self, terminal: TerminalId, bytes: &[u8]) -> PtyResult<()> {
        self.require_live(terminal)?;
        let generation = self.generation(terminal)?;
        self.record(PtyRequest::Write { terminal, generation, bytes: bytes.to_vec() });
        Ok(())
    }

    fn resize(&mut self, terminal: TerminalId, size: TerminalSize) -> PtyResult<()> {
        self.require_live(terminal)?;
        let generation = self.generation(terminal)?;
        self.record(PtyRequest::Resize { terminal, generation, size });
        Ok(())
    }

    fn kill(&mut self, terminal: TerminalId) -> PtyResult<()> {
        let generation = self.generation(terminal)?;
        self.record(PtyRequest::Kill { terminal, generation });
        Ok(())
    }

    fn release(&mut self, terminal: TerminalId) -> PtyResult<()> {
        let generation = self.generation(terminal)?;
        // Retire the terminal *before* publishing the request, not after. `record` is what
        // a test observes through `recv_request`, so recording first leaves a window where
        // the request is visible but the terminal is still live — and a test that emits the
        // moment it sees the release wins that race on some schedulers and not others.
        self.shared.0.lock().expect("scripted PTY state poisoned").live.remove(&terminal);
        self.record(PtyRequest::Release { terminal, generation });
        Ok(())
    }
}

impl ScriptedPty {
    fn generation(&self, terminal: TerminalId) -> PtyResult<TerminalGeneration> {
        self.shared
            .0
            .lock()
            .expect("scripted PTY state poisoned")
            .live
            .get(&terminal)
            .map(|(generation, _)| *generation)
            .ok_or(PtyError::UnknownTerminal(terminal))
    }

    fn require_live(&self, terminal: TerminalId) -> PtyResult<()> {
        if self.shared.0.lock().expect("scripted PTY state poisoned").live.contains_key(&terminal) {
            Ok(())
        } else {
            Err(PtyError::UnknownTerminal(terminal))
        }
    }
}

fn event_terminal(event: &PtyEvent) -> TerminalId {
    match event {
        PtyEvent::Spawned { terminal, .. }
        | PtyEvent::Output { terminal, .. }
        | PtyEvent::Exited { terminal, .. }
        | PtyEvent::Failed { terminal, .. } => *terminal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;
    use termesh_terminal::PtyWorker;

    fn spec() -> TerminalSpec {
        TerminalSpec {
            program: "helper".into(),
            args: vec!["--one".into()],
            cwd: "/proj".into(),
            env: Vec::new(),
        }
    }

    #[test]
    fn scripted_pty_records_requests_and_emits_only_for_live_terminals() {
        let pty = ScriptedPty::new();
        let control = pty.control();
        let (tx, rx) = mpsc::channel();
        let worker = PtyWorker::spawn(pty, move |event| {
            tx.send(event).unwrap();
        });
        let terminal = TerminalId::new(3);
        let generation = TerminalGeneration::new(1);
        assert!(!control.emit(PtyEvent::Output {
            terminal,
            generation,
            bytes: b"too early".to_vec(),
        }));

        let spawn = PtyRequest::Spawn {
            terminal,
            generation,
            spec: spec(),
            size: TerminalSize { rows: 24, cols: 80 },
        };
        assert!(worker.request(spawn.clone()));
        assert_eq!(control.recv_request(Duration::from_secs(1)), Some(spawn));
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            PtyEvent::Spawned { terminal: id, .. } if id == terminal
        ));

        assert!(control.emit(PtyEvent::Output { terminal, generation, bytes: b"ok\r\n".to_vec() }));
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            PtyEvent::Output { terminal: id, bytes, .. }
                if id == terminal && bytes == b"ok\r\n"
        ));

        assert!(worker.request(PtyRequest::Release { terminal, generation }));
        assert_eq!(
            control.recv_request(Duration::from_secs(1)),
            Some(PtyRequest::Release { terminal, generation })
        );
        assert!(!control.emit(PtyEvent::Output {
            terminal,
            generation,
            bytes: b"too late".to_vec(),
        }));
    }
}
