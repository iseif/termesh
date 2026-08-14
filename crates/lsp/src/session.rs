//! Language-server process transport and supervision.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use termesh_core::{LspEvent, LspFailure, LspFailureKind, LspRequest};

use crate::{encode_frame, FrameReader, LanguageService, Message, Recipe, Translator};

const STDERR_LINES: usize = 200;
const MAX_RESTARTS: usize = 3;

enum Work {
    Request(LspRequest),
    Bytes { generation: u64, bytes: Vec<u8> },
    Disconnected { generation: u64 },
}

struct Process {
    child: Child,
    stdin: ChildStdin,
    stdout: ChildStdout,
    stderr: ChildStderr,
}

struct Supervisor {
    command: Vec<String>,
    root: PathBuf,
    initialization_options: Option<String>,
    inbox: Receiver<Work>,
    work: Sender<Work>,
    log: Arc<Mutex<VecDeque<String>>>,
}

pub struct LspSession {
    work: Sender<Work>,
    log: Arc<Mutex<VecDeque<String>>>,
}

impl LspSession {
    pub fn spawn<F>(recipe: &Recipe, root: &Path, sink: F) -> std::io::Result<Self>
    where
        F: Fn(LspEvent) + Send + 'static,
    {
        let process = spawn_process(&recipe.command, root)?;
        let (work, inbox) = mpsc::channel();
        let log = Arc::new(Mutex::new(VecDeque::new()));
        let worker = work.clone();
        let supervisor = Supervisor {
            command: recipe.command.clone(),
            root: root.to_path_buf(),
            initialization_options: recipe.initialization_options.clone(),
            inbox,
            work: worker,
            log: log.clone(),
        };
        thread::Builder::new().name("termesh-lsp".into()).spawn(move || {
            supervise(process, supervisor, sink);
        })?;
        Ok(Self { work, log })
    }

    /// Connect the transport to in-memory streams. The production path uses [`spawn`];
    /// this path drives every protocol/transport behavior without an installed server.
    pub fn connect<F>(
        stdin: Box<dyn Write + Send>,
        stdout: Box<dyn Read + Send>,
        root: PathBuf,
        initialization_options: Option<String>,
        sink: F,
    ) -> Self
    where
        F: Fn(LspEvent) + Send + 'static,
    {
        let (work, inbox) = mpsc::channel();
        let log = Arc::new(Mutex::new(VecDeque::new()));
        let reader_work = work.clone();
        let _ = spawn_reader(0, stdout, reader_work);
        let worker_log = log.clone();
        let _ = thread::Builder::new().name("termesh-lsp".into()).spawn(move || {
            run_connection(0, stdin, root, initialization_options, &inbox, &worker_log, &sink);
        });
        Self { work, log }
    }

    pub fn server_log(&self) -> Vec<String> {
        self.log.lock().expect("LSP server log poisoned").iter().cloned().collect()
    }
}

impl LanguageService for LspSession {
    fn send(&mut self, request: LspRequest) {
        let _ = self.work.send(Work::Request(request));
    }

    fn poll(&mut self) -> Vec<LspEvent> {
        Vec::new()
    }
}

impl Drop for LspSession {
    fn drop(&mut self) {
        let _ = self.work.send(Work::Request(LspRequest::Shutdown));
    }
}

fn spawn_process(command: &[String], root: &Path) -> std::io::Result<Process> {
    let Some((program, arguments)) = command.split_first() else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "no language-server command configured",
        ));
    };
    let mut child = Command::new(program)
        .args(arguments)
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let missing = || std::io::Error::other("language-server pipes were not created");
    Ok(Process {
        stdin: child.stdin.take().ok_or_else(missing)?,
        stdout: child.stdout.take().ok_or_else(missing)?,
        stderr: child.stderr.take().ok_or_else(missing)?,
        child,
    })
}

fn supervise<F>(mut process: Process, supervisor: Supervisor, sink: F)
where
    F: Fn(LspEvent),
{
    let mut generation = 0;
    let mut restarts = 0;
    loop {
        sink(LspEvent::Started);
        let _ = spawn_reader(generation, Box::new(process.stdout), supervisor.work.clone());
        let _ = spawn_stderr(process.stderr, supervisor.log.clone());
        let outcome = run_connection(
            generation,
            Box::new(process.stdin),
            supervisor.root.clone(),
            supervisor.initialization_options.clone(),
            &supervisor.inbox,
            &supervisor.log,
            &sink,
        );
        match outcome {
            ConnectionEnd::Shutdown => {
                let _ = process.child.kill();
                let _ = process.child.wait();
                return;
            }
            ConnectionEnd::HandshakeFailed => {
                let _ = process.child.kill();
                let _ = process.child.wait();
                return;
            }
            ConnectionEnd::Disconnected => {
                let code = process.child.wait().ok().and_then(|status| status.code());
                sink(LspEvent::Exited { code });
            }
        }

        if restarts >= MAX_RESTARTS {
            sink(LspEvent::Unavailable {
                message: "language server exited repeatedly; automatic restart stopped".into(),
            });
            return;
        }
        thread::sleep(Duration::from_millis(50_u64 << restarts));
        restarts += 1;
        generation += 1;
        process = match spawn_process(&supervisor.command, &supervisor.root) {
            Ok(process) => process,
            Err(error) => {
                sink(failed(
                    LspFailureKind::Transport,
                    format!("could not restart server: {error}"),
                ));
                return;
            }
        };
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConnectionEnd {
    Shutdown,
    HandshakeFailed,
    Disconnected,
}

fn run_connection<F>(
    generation: u64,
    mut stdin: Box<dyn Write + Send>,
    root: PathBuf,
    initialization_options: Option<String>,
    inbox: &Receiver<Work>,
    log: &Arc<Mutex<VecDeque<String>>>,
    sink: &F,
) -> ConnectionEnd
where
    F: Fn(LspEvent),
{
    let mut translator = Translator::new();
    let initialize = match translator.initialize_with(root, initialization_options.as_deref()) {
        Ok(message) => message,
        Err(failure) => {
            sink(LspEvent::Failed { id: None, failure });
            return ConnectionEnd::HandshakeFailed;
        }
    };
    if write_message(&mut stdin, &initialize).is_err() {
        sink(failed(LspFailureKind::Transport, "could not reach language server"));
        return ConnectionEnd::Disconnected;
    }

    let mut frames = FrameReader::new();
    while let Ok(work) = inbox.recv() {
        match work {
            Work::Request(LspRequest::Shutdown) => {
                for message in translator.outgoing(LspRequest::Shutdown) {
                    let _ = write_message(&mut stdin, &message);
                }
                let _ = write_message(
                    &mut stdin,
                    &Message::Notification {
                        method: "exit".into(),
                        params: serde_json::Value::Null,
                    },
                );
                let _ = stdin.flush();
                return ConnectionEnd::Shutdown;
            }
            Work::Request(request) => {
                for message in translator.outgoing(request) {
                    if write_message(&mut stdin, &message).is_err() {
                        sink(failed(
                            LspFailureKind::Transport,
                            "language server stopped listening",
                        ));
                        return ConnectionEnd::Disconnected;
                    }
                }
            }
            Work::Bytes { generation: incoming, bytes } if incoming == generation => {
                frames.push(&bytes);
                while let Some(value) = frames.next_frame() {
                    let message = match Message::decode(value) {
                        Ok(message) => message,
                        Err(error) => {
                            push_log(log, error.to_string());
                            continue;
                        }
                    };
                    let (events, replies) = translator.incoming(message);
                    let handshake_failed = events.iter().any(|event| {
                        matches!(event, LspEvent::Failed { failure, .. }
                            if failure.kind == LspFailureKind::Handshake)
                    });
                    for event in events {
                        sink(event);
                    }
                    for reply in replies {
                        if write_message(&mut stdin, &reply).is_err() {
                            sink(failed(
                                LspFailureKind::Transport,
                                "language server stopped listening",
                            ));
                            return ConnectionEnd::Disconnected;
                        }
                    }
                    if handshake_failed {
                        return ConnectionEnd::HandshakeFailed;
                    }
                }
            }
            Work::Disconnected { generation: incoming } if incoming == generation => {
                return ConnectionEnd::Disconnected;
            }
            Work::Bytes { .. } | Work::Disconnected { .. } => {}
        }
    }
    ConnectionEnd::Shutdown
}

fn write_message(writer: &mut dyn Write, message: &Message) -> std::io::Result<()> {
    writer.write_all(&encode_frame(&message.encode()))?;
    writer.flush()
}

fn spawn_reader(
    generation: u64,
    mut stdout: Box<dyn Read + Send>,
    work: Sender<Work>,
) -> std::io::Result<()> {
    thread::Builder::new().name("termesh-lsp-in".into()).spawn(move || {
        let mut buffer = vec![0; 8 * 1024];
        loop {
            match stdout.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(count) => {
                    if work
                        .send(Work::Bytes { generation, bytes: buffer[..count].to_vec() })
                        .is_err()
                    {
                        return;
                    }
                }
            }
        }
        let _ = work.send(Work::Disconnected { generation });
    })?;
    Ok(())
}

fn spawn_stderr(stderr: ChildStderr, log: Arc<Mutex<VecDeque<String>>>) -> std::io::Result<()> {
    thread::Builder::new().name("termesh-lsp-err".into()).spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            push_log(&log, line);
        }
    })?;
    Ok(())
}

fn push_log(log: &Arc<Mutex<VecDeque<String>>>, line: String) {
    let mut log = log.lock().expect("LSP server log poisoned");
    if log.len() == STDERR_LINES {
        log.pop_front();
    }
    log.push_back(line);
}

fn failed(kind: LspFailureKind, message: impl Into<String>) -> LspEvent {
    LspEvent::Failed { id: None, failure: LspFailure { kind, message: message.into() } }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{encode_frame, LanguageService, Message};
    use std::io::{Read, Write};
    use std::path::PathBuf;
    use std::sync::mpsc::{self, Receiver, Sender};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::Duration;
    use termesh_core::{LspEvent, LspRequest};

    #[derive(Clone, Default)]
    struct Pipe(Arc<Mutex<Vec<u8>>>);

    impl Pipe {
        fn seen(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
        }
    }

    impl Write for Pipe {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    struct ScriptedStdout {
        chunks: Receiver<Vec<u8>>,
        current: Vec<u8>,
        offset: usize,
    }

    impl Read for ScriptedStdout {
        fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
            if self.offset >= self.current.len() {
                self.current = match self.chunks.recv() {
                    Ok(chunk) => chunk,
                    Err(_) => return Ok(0),
                };
                self.offset = 0;
            }
            let count = output.len().min(self.current.len() - self.offset);
            output[..count].copy_from_slice(&self.current[self.offset..self.offset + count]);
            self.offset += count;
            Ok(count)
        }
    }

    struct Harness {
        session: LspSession,
        written: Pipe,
        stdout: Sender<Vec<u8>>,
        events: Receiver<LspEvent>,
    }

    impl Harness {
        fn new() -> Self {
            let written = Pipe::default();
            let (stdout, chunks) = mpsc::channel();
            let (event_tx, events) = mpsc::channel();
            let session = LspSession::connect(
                Box::new(written.clone()),
                Box::new(ScriptedStdout { chunks, current: Vec::new(), offset: 0 }),
                PathBuf::from("/proj"),
                None,
                move |event| {
                    let _ = event_tx.send(event);
                },
            );
            Self { session, written, stdout, events }
        }

        fn wait_for_write(&self, needle: &str) {
            for _ in 0..500 {
                if self.written.seen().contains(needle) {
                    return;
                }
                thread::sleep(Duration::from_millis(10));
            }
            panic!("worker never wrote {needle:?}: {}", self.written.seen());
        }

        fn say(&self, message: Message) {
            self.stdout.send(encode_frame(&message.encode())).unwrap();
        }

        fn next_event(&self) -> LspEvent {
            self.events.recv_timeout(Duration::from_secs(5)).expect("expected LSP event")
        }

        fn ready(&self) {
            self.wait_for_write("initialize");
            self.say(Message::Response { id: 1, result: serde_json::json!({"capabilities":{}}) });
            assert_eq!(self.next_event(), LspEvent::Ready);
        }
    }

    #[test]
    fn the_handshake_precedes_and_then_releases_queued_requests() {
        let mut harness = Harness::new();
        harness.wait_for_write("initialize");
        harness.session.send(LspRequest::DidOpen {
            path: "/proj/src/main.rs".into(),
            language_id: "rust".into(),
            version: 1,
            text: "fn main() {}".into(),
        });
        assert!(!harness.written.seen().contains("textDocument/didOpen"));

        harness.say(Message::Response { id: 1, result: serde_json::json!({"capabilities":{}}) });
        assert_eq!(harness.next_event(), LspEvent::Ready);
        harness.wait_for_write("textDocument/didOpen");
    }

    #[test]
    fn unsolicited_diagnostics_reach_the_sink() {
        let harness = Harness::new();
        harness.ready();
        harness.say(Message::Notification {
            method: "textDocument/publishDiagnostics".into(),
            params: serde_json::json!({
                "uri":"file:///proj/src/main.rs",
                "diagnostics":[]
            }),
        });
        assert!(matches!(harness.next_event(), LspEvent::Diagnostics { .. }));
    }

    #[test]
    fn connect_poll_is_empty_because_events_use_the_sink() {
        let mut harness = Harness::new();
        assert!(harness.session.poll().is_empty());
    }
}
