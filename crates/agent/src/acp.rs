//! The ACP client transport (ADR-0007 §2).
//!
//! A thin shell around [`crate::protocol::Translator`], which holds all the protocol
//! logic and none of the I/O. This file owns only the parts that cannot be tested without
//! a process: spawning the agent, three threads, and the channel back into the app.
//!
//! Follows ADR-0005's worker template rather than deviating from it — blocking calls on a
//! dedicated thread, results delivered as `AppMessage`. One subprocess with two pipes is
//! not a concurrency problem that needs a reactor, which is why `tokio` is still not in
//! the tree.
//!
//! ```text
//!   stdout reader ──┐
//!                   ├─► worker (owns Translator + stdin) ──► sink ──► AppMessage::Agent
//!   AgentRequest ───┘
//!   stderr drain ─────► log            (drained, never blocked — see `spawn`)
//! ```

use std::ffi::OsStr;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Sender};
use std::thread;

use termesh_core::{AgentEvent, AgentRequest, SessionId};

use crate::jsonrpc::{DecodeError, Message};
use crate::protocol::Translator;
use crate::service::{AgentIntegration, AgentService, ClientCapabilities};

/// Either direction of traffic, funnelled into the one thread that owns the translator.
enum Work {
    /// Something the app wants sent.
    Request(AgentRequest),
    /// A line the agent wrote.
    Line(String),
    /// The agent's stdout closed — it has exited or stopped talking.
    Disconnected,
}

/// A running ACP agent subprocess.
pub struct AcpAgent {
    work: Sender<Work>,
    child: Option<Child>,
    capabilities: ClientCapabilities,
}

impl AcpAgent {
    /// Spawn `command` and connect to it.
    ///
    /// `command` is an argv array — never a shell string, and never assembled into one
    /// (ARCHITECTURE.md §11). `sink` receives every event; the app wraps it in
    /// `AppMessage::Agent` so agent traffic wakes the main loop exactly as filesystem
    /// traffic does.
    pub fn spawn<S, F>(
        command: &[S],
        cwd: &Path,
        capabilities: ClientCapabilities,
        sink: F,
    ) -> std::io::Result<Self>
    where
        S: AsRef<OsStr>,
        F: Fn(AgentEvent) + Send + 'static,
    {
        let Some((program, args)) = command.split_first() else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "no agent command configured",
            ));
        };

        let mut child = Command::new(program)
            .args(args)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        // Piped above, so these are present — but a panic in a library path is worse
        // than an error the caller can degrade on.
        let missing = || std::io::Error::other("agent pipes were not created");
        let stdin = child.stdin.take().ok_or_else(missing)?;
        let stdout = child.stdout.take().ok_or_else(missing)?;
        let stderr = child.stderr.take().ok_or_else(missing)?;

        // Agents log to stderr, and a pipe nobody reads fills up and blocks the child
        // mid-turn. Drain it unconditionally; the content is diagnostics, not protocol.
        thread::Builder::new().name("termesh-acp-err".into()).spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                tracing_line(&line);
            }
        })?;

        let mut agent =
            Self::connect(Box::new(stdin), Box::new(BufReader::new(stdout)), capabilities, sink);
        agent.child = Some(child);
        Ok(agent)
    }

    /// Connect over arbitrary streams.
    ///
    /// Exists so the transport can be driven by in-memory pipes in tests: everything
    /// except `spawn` is then exercised without an agent installed.
    pub fn connect<F>(
        mut stdin: Box<dyn Write + Send>,
        stdout: Box<dyn BufRead + Send>,
        capabilities: ClientCapabilities,
        sink: F,
    ) -> Self
    where
        F: Fn(AgentEvent) + Send + 'static,
    {
        let (work, inbox) = mpsc::channel::<Work>();

        // Reader: lines in, straight to the worker. Deliberately does no parsing, so a
        // slow translator can never stall the pipe.
        let reader_work = work.clone();
        let _ = thread::Builder::new().name("termesh-acp-in".into()).spawn(move || {
            for line in stdout.lines() {
                let Ok(line) = line else { break };
                if reader_work.send(Work::Line(line)).is_err() {
                    return; // the app is gone
                }
            }
            let _ = reader_work.send(Work::Disconnected);
        });

        // Worker: the only thread that touches the translator or writes to stdin, so
        // neither needs a lock.
        let _ = thread::Builder::new().name("termesh-acp".into()).spawn(move || {
            let mut translator = Translator::new();

            // The handshake goes out before anything else; requests that arrive during it
            // are queued by the translator rather than dropped.
            let hello = translator.initialize(capabilities);
            if stdin.write_all(hello.encode().as_bytes()).is_err() {
                sink(failed("could not reach the agent"));
                return;
            }
            let _ = stdin.flush();

            while let Ok(item) = inbox.recv() {
                let outgoing = match item {
                    Work::Request(AgentRequest::Shutdown) => break,
                    Work::Request(request) => translator.outgoing(request),
                    Work::Line(line) => match Message::decode(&line) {
                        Ok(message) => {
                            let (events, replies) = translator.incoming(message);
                            for event in events {
                                sink(event);
                            }
                            replies
                        }
                        // Chatty agents are common; a stray line is traffic to skip, not
                        // a reason to end the session.
                        Err(DecodeError::NotJson(_)) => {
                            tracing_line(&line);
                            continue;
                        }
                        Err(e) => {
                            tracing_line(&e.to_string());
                            continue;
                        }
                    },
                    Work::Disconnected => {
                        // The one failure a user must never experience as a hang: the
                        // agent died and the turn will never end on its own.
                        sink(failed("the agent exited"));
                        break;
                    }
                };

                for message in outgoing {
                    if stdin.write_all(message.encode().as_bytes()).is_err() {
                        sink(failed("the agent stopped listening"));
                        return;
                    }
                }
                let _ = stdin.flush();
            }
        });

        Self { work, child: None, capabilities }
    }
}

fn failed(message: &str) -> AgentEvent {
    // Session 0 means "no particular session": the model surfaces it and clears any
    // in-flight turn rather than waiting forever.
    AgentEvent::Failed { session: SessionId::new(0), message: message.to_string() }
}

/// Agent stderr is local diagnostic data. With no application subscriber this compiles
/// down to an unobserved event; `--trace FILE` is the only route that records it.
fn tracing_line(line: &str) {
    tracing::trace!(target: "termesh::agent::acp", line, "agent stderr");
}

impl AgentService for AcpAgent {
    fn integration(&self) -> AgentIntegration {
        AgentIntegration::Acp
    }

    fn capabilities(&self) -> ClientCapabilities {
        self.capabilities
    }

    fn send(&mut self, request: AgentRequest) {
        // A dead worker means the agent is gone; the Failed event already went out, so
        // dropping the request here is right rather than panicking on a closed channel.
        let _ = self.work.send(Work::Request(request));
    }

    /// Always empty.
    ///
    /// Events reach the app through the sink, not by polling: the main loop blocks on its
    /// message channel, so a client that only answered when asked would never be asked.
    /// The scripted agent uses `poll`; both end at `Model::on_agent_event`.
    fn poll(&mut self) -> Vec<AgentEvent> {
        Vec::new()
    }
}

impl Drop for AcpAgent {
    fn drop(&mut self) {
        let _ = self.work.send(Work::Request(AgentRequest::Shutdown));
        // Killing outright rather than waiting politely: the editor is exiting, and an
        // orphaned model process outliving it is a real cost to the user. Closing stdin
        // first and giving the agent a grace period is the nicer shutdown, and belongs
        // here once there is somewhere to report a hung agent to.
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::Receiver;
    use std::time::Duration;

    /// A pipe whose written bytes can be read back, so a test can play the agent.
    #[derive(Clone)]
    struct Pipe(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl Pipe {
        fn new() -> Self {
            Self(Default::default())
        }
        /// Everything written so far, without consuming it — draining would make
        /// assertions depend on when the worker happened to flush.
        fn seen(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    impl Write for Pipe {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Agent stdout the test drives line by line.
    ///
    /// A `Cursor` would hit EOF the instant it was drained, so the worker could see the
    /// disconnect before the test had sent anything — every test would race the shutdown.
    /// Here EOF happens only when the test drops the sender.
    struct ScriptedStdout {
        lines: Receiver<String>,
        buf: Vec<u8>,
        pos: usize,
    }

    impl std::io::Read for ScriptedStdout {
        fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
            if self.pos >= self.buf.len() {
                match self.lines.recv() {
                    Ok(line) => {
                        self.buf = line.into_bytes();
                        self.pos = 0;
                    }
                    Err(_) => return Ok(0), // the test dropped the sender: EOF
                }
            }
            let n = (self.buf.len() - self.pos).min(out.len());
            out[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
            self.pos += n;
            Ok(n)
        }
    }

    struct Harness {
        agent: AcpAgent,
        written: Pipe,
        events: Receiver<AgentEvent>,
        stdout: Option<Sender<String>>,
    }

    impl Harness {
        fn new() -> Self {
            let written = Pipe::new();
            let (tx, events) = mpsc::channel();
            let (stdout, lines) = mpsc::channel::<String>();

            let agent = AcpAgent::connect(
                Box::new(written.clone()),
                Box::new(BufReader::new(ScriptedStdout { lines, buf: Vec::new(), pos: 0 })),
                ClientCapabilities::default(),
                move |event| {
                    let _ = tx.send(event);
                },
            );
            Self { agent, written, events, stdout: Some(stdout) }
        }

        /// Let the agent say something.
        fn say(&self, line: &str) {
            self.stdout.as_ref().unwrap().send(format!("{line}\n")).unwrap();
        }

        /// Close the agent's stdout, as an exiting process would.
        fn hang_up(&mut self) {
            self.stdout = None;
        }

        fn next_event(&self) -> AgentEvent {
            self.events.recv_timeout(Duration::from_secs(5)).expect("expected an event")
        }

        /// Wait until the agent has written something matching `needle`.
        fn wrote(&self, needle: &str) -> bool {
            self.wait_for(needle, 500)
        }

        /// As [`Self::wrote`], but giving up quickly — for asserting something has *not*
        /// been written yet.
        fn wrote_quickly(&self, needle: &str) -> bool {
            self.wait_for(needle, 10)
        }

        fn wait_for(&self, needle: &str, tries: usize) -> bool {
            for _ in 0..tries {
                if self.written.seen().contains(needle) {
                    return true;
                }
                thread::sleep(Duration::from_millis(10));
            }
            false
        }

        fn written(&self) -> String {
            self.written.seen()
        }
    }

    #[test]
    fn the_handshake_goes_out_before_anything_else() {
        let h = Harness::new();
        assert!(h.wrote("\"method\":\"initialize\""));
        assert!(h.written().contains("readTextFile"));
    }

    /// A user who starts a session the instant the app opens must not lose it.
    #[test]
    fn requests_sent_during_the_handshake_are_flushed_once_it_completes() {
        let mut h = Harness::new();
        assert!(h.wrote("initialize"));

        h.agent.send(AgentRequest::NewSession { cwd: "/proj".into() });
        assert!(!h.wrote_quickly("session/new"), "nothing goes out before the agent replies");

        h.say(r#"{"jsonrpc":"2.0","id":1,"result":{}}"#);
        assert!(h.wrote("session/new"), "and it is sent once we are ready");
    }

    #[test]
    fn a_session_reaches_the_sink_with_our_own_id() {
        let mut h = Harness::new();
        assert!(h.wrote("initialize"));
        h.say(r#"{"jsonrpc":"2.0","id":1,"result":{}}"#);
        assert!(matches!(h.next_event(), AgentEvent::Ready { .. }));

        h.agent.send(AgentRequest::NewSession { cwd: "/proj".into() });
        assert!(h.wrote("session/new"));
        h.say(r#"{"jsonrpc":"2.0","id":2,"result":{"sessionId":"s-1"}}"#);

        assert!(matches!(h.next_event(), AgentEvent::SessionStarted { .. }));
    }

    #[test]
    fn streamed_text_reaches_the_sink() {
        let mut h = Harness::new();
        assert!(h.wrote("initialize"));
        h.say(r#"{"jsonrpc":"2.0","id":1,"result":{}}"#);
        assert!(matches!(h.next_event(), AgentEvent::Ready { .. }));
        h.agent.send(AgentRequest::NewSession { cwd: "/proj".into() });
        assert!(h.wrote("session/new"));
        h.say(r#"{"jsonrpc":"2.0","id":2,"result":{"sessionId":"s-1"}}"#);
        assert!(matches!(h.next_event(), AgentEvent::SessionStarted { .. }));

        h.say(
            r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s-1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"hi"}}}}"#,
        );
        match h.next_event() {
            AgentEvent::MessageChunk { text, .. } => assert_eq!(text, "hi"),
            other => panic!("expected streamed text, got {other:?}"),
        }
    }

    /// The failure a user must never experience as a hang.
    #[test]
    fn a_dead_agent_is_reported_rather_than_hanging() {
        let mut h = Harness::new();
        h.hang_up();
        match h.next_event() {
            AgentEvent::Failed { message, .. } => assert!(message.contains("exited"), "{message}"),
            other => panic!("expected a failure, got {other:?}"),
        }
    }

    #[test]
    fn noise_on_stdout_does_not_end_the_session() {
        let mut h = Harness::new();
        h.say("Listening on stdio...");
        h.say("not json at all");
        h.say(r#"{"hello":"world"}"#);
        h.say(r#"{"jsonrpc":"2.0","id":1,"result":{}}"#);

        // The noise was skipped; the handshake still completed, so a queued request flows.
        h.agent.send(AgentRequest::NewSession { cwd: "/proj".into() });
        assert!(h.wrote("session/new"), "a chatty agent is still a working agent");
    }

    #[test]
    fn a_response_we_never_asked_for_is_ignored() {
        let mut h = Harness::new();
        assert!(h.wrote("initialize"));
        h.say(r#"{"jsonrpc":"2.0","id":9999,"result":{"sessionId":"ghost"}}"#);
        h.say(r#"{"jsonrpc":"2.0","id":1,"result":{}}"#);

        h.agent.send(AgentRequest::NewSession { cwd: "/proj".into() });
        assert!(h.wrote("session/new"), "the stray response did not derail us");
    }

    #[test]
    fn spawning_with_no_command_is_an_error_not_a_panic() {
        let empty: [&str; 0] = [];
        let result = AcpAgent::spawn(&empty, Path::new("."), ClientCapabilities::default(), |_| {});
        assert!(result.is_err());
    }

    #[test]
    fn spawning_a_missing_binary_reports_the_error() {
        let result = AcpAgent::spawn(
            &["definitely-not-a-real-agent-binary"],
            Path::new("."),
            ClientCapabilities::default(),
            |_| {},
        );
        assert!(result.is_err(), "a missing agent must not take the editor down");
    }

    #[test]
    fn the_transport_reports_itself_as_tier_one() {
        let mut h = Harness::new();
        assert_eq!(h.agent.integration(), AgentIntegration::Acp);
        assert!(h.agent.poll().is_empty(), "events arrive through the sink, not by polling");
    }
}
