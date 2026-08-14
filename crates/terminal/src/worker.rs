//! A single owner thread for blocking [`crate::PtyService`] calls (ADR-0005, ADR-0008).

use std::sync::mpsc::{self, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;

use termesh_core::{PtyEvent, PtyRequest, TerminalGeneration, TerminalId};

use crate::{PtyEventSink, PtyResult, PtyService};

enum WorkerMessage {
    Request(PtyRequest),
    Shutdown,
}

pub struct PtyWorker {
    tx: Sender<WorkerMessage>,
    handle: Option<JoinHandle<()>>,
}

impl PtyWorker {
    pub fn spawn<S, F>(mut service: S, sink: F) -> Self
    where
        S: PtyService,
        F: Fn(PtyEvent) + Send + Sync + 'static,
    {
        let sink: PtyEventSink = Arc::new(sink);
        let (tx, rx) = mpsc::channel();
        let handle = std::thread::Builder::new()
            .name("termesh-pty".into())
            .spawn(move || {
                while let Ok(message) = rx.recv() {
                    let WorkerMessage::Request(request) = message else {
                        break;
                    };
                    let terminal = request_terminal(&request);
                    let generation = request_generation(&request);
                    if let Err(error) = dispatch(&mut service, request, sink.clone()) {
                        sink(PtyEvent::Failed { terminal, generation, message: error.to_string() });
                    }
                }
            })
            .expect("spawning the PTY worker thread");
        Self { tx, handle: Some(handle) }
    }

    pub fn request(&self, request: PtyRequest) -> bool {
        self.tx.send(WorkerMessage::Request(request)).is_ok()
    }
}

fn request_generation(request: &PtyRequest) -> TerminalGeneration {
    match request {
        PtyRequest::Spawn { generation, .. }
        | PtyRequest::Write { generation, .. }
        | PtyRequest::Resize { generation, .. }
        | PtyRequest::Kill { generation, .. }
        | PtyRequest::Release { generation, .. } => *generation,
    }
}

impl Drop for PtyWorker {
    fn drop(&mut self) {
        let _ = self.tx.send(WorkerMessage::Shutdown);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn request_terminal(request: &PtyRequest) -> TerminalId {
    match request {
        PtyRequest::Spawn { terminal, .. }
        | PtyRequest::Write { terminal, .. }
        | PtyRequest::Resize { terminal, .. }
        | PtyRequest::Kill { terminal, .. }
        | PtyRequest::Release { terminal, .. } => *terminal,
    }
}

fn dispatch<S: PtyService>(
    service: &mut S,
    request: PtyRequest,
    sink: PtyEventSink,
) -> PtyResult<()> {
    match request {
        PtyRequest::Spawn { terminal, generation, spec, size } => {
            service.spawn(terminal, generation, spec, size, sink)
        }
        PtyRequest::Write { terminal, bytes, .. } => service.write(terminal, &bytes),
        PtyRequest::Resize { terminal, size, .. } => service.resize(terminal, size),
        PtyRequest::Kill { terminal, .. } => service.kill(terminal),
        PtyRequest::Release { terminal, .. } => service.release(terminal),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PtyError;
    use std::collections::BTreeMap;
    use std::sync::mpsc;
    use std::time::Duration;
    use termesh_core::{PtyEvent, PtyRequest, TerminalId, TerminalSize, TerminalSpec};

    #[derive(Default)]
    struct StubPty {
        sinks: BTreeMap<TerminalId, PtyEventSink>,
    }

    impl PtyService for StubPty {
        fn spawn(
            &mut self,
            terminal: TerminalId,
            generation: TerminalGeneration,
            _spec: TerminalSpec,
            _size: TerminalSize,
            sink: PtyEventSink,
        ) -> PtyResult<()> {
            self.sinks.insert(terminal, sink.clone());
            sink(PtyEvent::Spawned { terminal, generation, process_id: Some(42) });
            sink(PtyEvent::Output { terminal, generation, bytes: b"ready\r\n".to_vec() });
            Ok(())
        }

        fn write(&mut self, terminal: TerminalId, _bytes: &[u8]) -> PtyResult<()> {
            if self.sinks.contains_key(&terminal) {
                Ok(())
            } else {
                Err(PtyError::UnknownTerminal(terminal))
            }
        }

        fn resize(&mut self, _terminal: TerminalId, _size: TerminalSize) -> PtyResult<()> {
            Ok(())
        }

        fn kill(&mut self, _terminal: TerminalId) -> PtyResult<()> {
            Ok(())
        }

        fn release(&mut self, _terminal: TerminalId) -> PtyResult<()> {
            Ok(())
        }
    }

    fn spec() -> TerminalSpec {
        TerminalSpec {
            program: "helper".into(),
            args: Vec::new(),
            cwd: "/proj".into(),
            env: Vec::new(),
        }
    }

    #[test]
    fn worker_forwards_spawn_and_output_without_blocking_the_model() {
        let (tx, rx) = mpsc::channel();
        let worker = PtyWorker::spawn(StubPty::default(), move |event| {
            tx.send(event).unwrap();
        });
        let id = TerminalId::new(7);
        assert!(worker.request(PtyRequest::Spawn {
            terminal: id,
            generation: TerminalGeneration::new(1),
            spec: spec(),
            size: TerminalSize { rows: 24, cols: 80 },
        }));
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            PtyEvent::Spawned { terminal, process_id: Some(42), .. } if terminal == id
        ));
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            PtyEvent::Output { terminal, bytes, .. } if terminal == id && bytes == b"ready\r\n"
        ));
    }

    #[test]
    fn synchronous_service_errors_become_terminal_events() {
        let (tx, rx) = mpsc::channel();
        let worker = PtyWorker::spawn(StubPty::default(), move |event| {
            tx.send(event).unwrap();
        });
        let id = TerminalId::new(99);
        assert!(worker.request(PtyRequest::Write {
            terminal: id,
            generation: TerminalGeneration::new(1),
            bytes: b"x".to_vec(),
        }));
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            PtyEvent::Failed { terminal, message, .. }
                if terminal == id && message.contains("unknown terminal")
        ));
    }
}
