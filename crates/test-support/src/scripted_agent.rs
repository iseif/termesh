//! A scripted ACP agent — the fake that makes the review loop testable (ADR-0007 §7).
//!
//! Lands *before* the real client, because it is what the real client is tested against:
//! no subprocess, no pipes, no timing, and no network. ARCHITECTURE.md §18 asks for
//! "scripted ACP agent replaying `session/update` streams incl. edit proposals and
//! tool-permission requests", and CONTRIBUTING.md's fakes invariant makes it non-optional.
//!
//! Scripts are written in terms of what the *agent* does, not in terms of wire messages,
//! so a test reads like the interaction it is describing:
//!
//! ```
//! use termesh_test_support::{ScriptedAgent, ScriptedUpdate};
//! use termesh_agent::{AgentRequest, AgentService};
//!
//! let mut agent = ScriptedAgent::new().with_turn(vec![
//!     ScriptedUpdate::Message("Renaming it.".into()),
//!     ScriptedUpdate::ReadFile("/proj/main.rs".into()),
//!     ScriptedUpdate::Edit {
//!         path: "/proj/main.rs".into(),
//!         old_text: Some("fn main() {}\n".into()),
//!         new_text: "fn run() {}\n".into(),
//!     },
//!     ScriptedUpdate::End,
//! ]);
//! agent.send(AgentRequest::NewSession { cwd: "/proj".into() });
//! assert!(!agent.poll().is_empty());
//! ```

use std::collections::VecDeque;
use std::path::PathBuf;

use termesh_agent::service::{
    AgentEvent, AgentIntegration, AgentRequest, AgentService, StopReason,
};
use termesh_core::{
    AgentCapabilities, PermissionRequestId, ProposalId, ProposedEditDiff, ReadRequestId, SessionId,
    SessionMode,
};

/// One thing the scripted agent does, in the order a real turn would do it.
///
/// Session and proposal ids are filled in at replay time, so scripts stay readable and
/// do not have to predict identifiers the client hands out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScriptedUpdate {
    /// Streamed assistant text.
    Message(String),
    /// Streamed reasoning.
    Thought(String),
    /// Ask the client for a file. **Replay pauses here** until the client answers, which
    /// is what makes the read/propose ordering in ADR-0007 §5 testable at all.
    ReadFile(PathBuf),
    /// Propose an edit, as whole-file before/after — the shape ACP actually uses.
    Edit { path: PathBuf, old_text: Option<String>, new_text: String },
    /// Write a file through the client — what an agent does when it edits. Carries no
    /// base text, because the client owns the buffer and the agent does not.
    Write { path: PathBuf, content: String },
    /// Ask permission to run a command.
    Permission { summary: String, command: Vec<String> },
    /// Ask permission to *edit a file*, describing the change — what Codex in `read-only`
    /// and opencode under `permission.edit: "ask"` do. `old_text` is deliberately free-form:
    /// pass the whole document to model opencode, or just the touched lines to model Codex,
    /// because the client has to handle both (ADR-0016 §1a).
    EditPermission { summary: String, path: PathBuf, old_text: String, new_text: String },
    /// End the turn normally.
    End,
    /// End the turn some other way.
    Stop(StopReason),
    /// Fail the turn.
    Fail(String),
}

/// An [`AgentService`] that replays a recorded stream.
#[derive(Debug, Default)]
pub struct ScriptedAgent {
    /// One entry per prompt, in order.
    turns: VecDeque<Vec<ScriptedUpdate>>,
    /// The remainder of the current turn, parked while we wait for a file.
    resume: Option<Vec<ScriptedUpdate>>,
    outbox: VecDeque<AgentEvent>,
    /// Everything the client sent, for assertions.
    sent: Vec<AgentRequest>,
    /// File contents the client served, in order — the evidence that the agent is
    /// reading *our buffers* rather than the disk.
    served: Vec<(PathBuf, Option<String>)>,
    session: Option<SessionId>,
    /// Modes this agent claims to offer, reported when the session opens. Empty is the
    /// common case and means the agent has no choice to give (ADR-0015 §4).
    modes: Vec<SessionMode>,
    current_mode: Option<String>,
    next_id: u64,
    capabilities: AgentCapabilities,
    ready_emitted: bool,
}

impl ScriptedAgent {
    pub fn new() -> Self {
        Self::default()
    }

    /// Queue a turn. The first prompt replays the first turn, and so on.
    pub fn with_turn(mut self, updates: Vec<ScriptedUpdate>) -> Self {
        self.turns.push_back(updates);
        self
    }

    /// Offer session modes, the way Codex does. `current` must name one of them; it is
    /// the mode the session starts in, and the client is expected to leave it alone until
    /// a human says otherwise.
    pub fn with_modes(mut self, current: &str, modes: Vec<SessionMode>) -> Self {
        self.current_mode = Some(current.to_string());
        self.modes = modes;
        self
    }

    /// Set what the fake connection reports during its handshake. The first poll emits
    /// `Ready` exactly once, matching the protocol-neutral event the real ACP transport
    /// produces before any session behavior (ADR-0014 §4).
    pub fn with_capabilities(mut self, capabilities: AgentCapabilities) -> Self {
        self.capabilities = capabilities;
        self
    }

    /// Every request the client has sent.
    pub fn sent(&self) -> &[AgentRequest] {
        &self.sent
    }

    /// The file contents the client served, in the order they were asked for.
    pub fn served(&self) -> &[(PathBuf, Option<String>)] {
        &self.served
    }

    /// Whether the script has been fully consumed.
    pub fn is_exhausted(&self) -> bool {
        self.turns.is_empty() && self.resume.is_none()
    }

    fn fresh_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    /// Emit updates until the script ends or asks for a file.
    fn replay(&mut self, mut updates: Vec<ScriptedUpdate>) {
        let Some(session) = self.session else {
            // A prompt with no session is a client bug; surface it rather than hang.
            self.outbox.push_back(AgentEvent::Failed {
                session: SessionId::new(0),
                message: "prompt before session/new".into(),
            });
            return;
        };

        while !updates.is_empty() {
            let update = updates.remove(0);
            match update {
                ScriptedUpdate::Message(text) => {
                    self.outbox.push_back(AgentEvent::MessageChunk { session, text })
                }
                ScriptedUpdate::Thought(text) => {
                    self.outbox.push_back(AgentEvent::ThoughtChunk { session, text })
                }
                ScriptedUpdate::ReadFile(path) => {
                    let request = ReadRequestId::new(self.fresh_id());
                    self.outbox.push_back(AgentEvent::ReadFileRequested { session, request, path });
                    // Park the rest: a real agent cannot propose an edit to a file it has
                    // not read back yet, and tests should not be able to pretend it can.
                    self.resume = Some(updates);
                    return;
                }
                ScriptedUpdate::Edit { path, old_text, new_text } => {
                    let proposal = ProposalId::new(self.fresh_id());
                    self.outbox.push_back(AgentEvent::ProposedEdit {
                        session,
                        proposal,
                        path,
                        old_text,
                        new_text,
                    });
                }
                ScriptedUpdate::Write { path, content } => {
                    let proposal = ProposalId::new(self.fresh_id());
                    self.outbox.push_back(AgentEvent::ProposedEdit {
                        session,
                        proposal,
                        path,
                        old_text: None,
                        new_text: content,
                    });
                }
                ScriptedUpdate::Permission { summary, command } => {
                    let request = PermissionRequestId::new(self.fresh_id());
                    self.outbox.push_back(AgentEvent::PermissionRequested {
                        session,
                        request,
                        summary,
                        command,
                        terminal_spec: None,
                        edit: None,
                    });
                }
                ScriptedUpdate::EditPermission { summary, path, old_text, new_text } => {
                    let request = PermissionRequestId::new(self.fresh_id());
                    self.outbox.push_back(AgentEvent::PermissionRequested {
                        session,
                        request,
                        summary,
                        command: Vec::new(),
                        terminal_spec: None,
                        edit: Some(ProposedEditDiff { path, old_text, new_text }),
                    });
                }
                ScriptedUpdate::End => self
                    .outbox
                    .push_back(AgentEvent::TurnEnded { session, reason: StopReason::EndTurn }),
                ScriptedUpdate::Stop(reason) => {
                    self.outbox.push_back(AgentEvent::TurnEnded { session, reason })
                }
                ScriptedUpdate::Fail(message) => {
                    self.outbox.push_back(AgentEvent::Failed { session, message })
                }
            }
        }
    }
}

impl AgentService for ScriptedAgent {
    fn integration(&self) -> AgentIntegration {
        AgentIntegration::Acp
    }

    fn send(&mut self, request: AgentRequest) {
        self.sent.push(request.clone());

        match request {
            AgentRequest::NewSession { .. } => {
                let session = SessionId::new(self.fresh_id());
                self.session = Some(session);
                self.outbox.push_back(AgentEvent::SessionStarted { session });
                if let Some(current) = self.current_mode.clone() {
                    self.outbox.push_back(AgentEvent::ModesAvailable {
                        session,
                        current,
                        available: self.modes.clone(),
                    });
                }
            }
            AgentRequest::Prompt { .. } => {
                let turn = self.turns.pop_front().unwrap_or_default();
                self.replay(turn);
            }
            AgentRequest::FileContents { path, contents, .. } => {
                self.served.push((path, contents));
                if let Some(rest) = self.resume.take() {
                    self.replay(rest);
                }
            }
            // `ModeChanged` is what the real translator emits once the agent has answered —
            // from the success reply, or from a later `current_mode_update` (ADR-0015 §5).
            // This double stands in for the translator, so it emits the same event; which
            // of the two wire paths produced it is settled in the protocol tests, because
            // real agents differ (codex-acp replies and never notifies).
            AgentRequest::SetMode { session, mode } => {
                self.current_mode = Some(mode.clone());
                self.outbox.push_back(AgentEvent::ModeChanged { session, mode });
            }
            AgentRequest::Cancel { session } => {
                self.resume = None;
                self.outbox
                    .push_back(AgentEvent::TurnEnded { session, reason: StopReason::Cancelled });
            }
            AgentRequest::Permission { .. }
            | AgentRequest::PermissionCancelled { .. }
            | AgentRequest::TerminalResponse { .. }
            | AgentRequest::Shutdown => {}
        }
    }

    fn poll(&mut self) -> Vec<AgentEvent> {
        let mut events = Vec::new();
        if !self.ready_emitted {
            self.ready_emitted = true;
            events.push(AgentEvent::Ready { capabilities: self.capabilities });
        }
        events.extend(self.outbox.drain(..));
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use termesh_core::{AgentCapabilities, PromptCapabilities};

    fn started() -> ScriptedAgent {
        let mut agent = ScriptedAgent::new();
        agent.send(AgentRequest::NewSession { cwd: PathBuf::from("/proj") });
        agent
    }

    fn session_of(agent: &mut ScriptedAgent) -> SessionId {
        let events = agent.poll();
        events
            .iter()
            .find_map(|event| match event {
                AgentEvent::SessionStarted { session } => Some(*session),
                _ => None,
            })
            .unwrap_or_else(|| panic!("expected a session, got {events:?}"))
    }

    fn prompt(agent: &mut ScriptedAgent, session: SessionId) -> Vec<AgentEvent> {
        agent.send(AgentRequest::Prompt {
            session,
            text: "do the thing".into(),
            context: String::new(),
        });
        agent.poll()
    }

    #[test]
    fn a_session_starts_before_anything_else_happens() {
        let mut agent = started();
        assert!(matches!(
            agent.poll().as_slice(),
            [AgentEvent::Ready { .. }, AgentEvent::SessionStarted { .. }]
        ));
    }

    #[test]
    fn negotiated_capabilities_reach_fake_driven_model_tests() {
        // The fake must cross the same protocol-neutral boundary as the real ACP
        // connection. Otherwise model tests can never exercise ADR-0014's handshake
        // state without depending on JSON-RPC transport details.
        let capabilities = AgentCapabilities {
            load_session: true,
            prompt_capabilities: PromptCapabilities {
                image: true,
                audio: false,
                embedded_context: true,
            },
        };
        let mut agent = ScriptedAgent::new().with_capabilities(capabilities);
        assert!(matches!(
            agent.poll().as_slice(),
            [AgentEvent::Ready { capabilities: actual }] if *actual == capabilities
        ));
    }

    #[test]
    fn a_turn_replays_in_order() {
        let mut agent = ScriptedAgent::new().with_turn(vec![
            ScriptedUpdate::Thought("thinking".into()),
            ScriptedUpdate::Message("hello".into()),
            ScriptedUpdate::End,
        ]);
        agent.send(AgentRequest::NewSession { cwd: PathBuf::from("/proj") });
        let session = session_of(&mut agent);

        let events = prompt(&mut agent, session);
        assert!(matches!(
            events.as_slice(),
            [
                AgentEvent::ThoughtChunk { .. },
                AgentEvent::MessageChunk { .. },
                AgentEvent::TurnEnded { reason: StopReason::EndTurn, .. }
            ]
        ));
    }

    /// The ordering ADR-0007 §5 depends on: the agent cannot propose an edit to a file it
    /// has not read back, so replay parks until the client answers.
    #[test]
    fn a_read_pauses_the_turn_until_the_client_answers() {
        let mut agent = ScriptedAgent::new().with_turn(vec![
            ScriptedUpdate::ReadFile(PathBuf::from("/proj/main.rs")),
            ScriptedUpdate::Edit {
                path: PathBuf::from("/proj/main.rs"),
                old_text: Some("fn main() {}\n".into()),
                new_text: "fn run() {}\n".into(),
            },
            ScriptedUpdate::End,
        ]);
        agent.send(AgentRequest::NewSession { cwd: PathBuf::from("/proj") });
        let session = session_of(&mut agent);

        let events = prompt(&mut agent, session);
        assert!(
            matches!(events.as_slice(), [AgentEvent::ReadFileRequested { .. }]),
            "the turn stops at the read, got {events:?}"
        );

        agent.send(AgentRequest::FileContents {
            session,
            request: ReadRequestId::new(1),
            path: PathBuf::from("/proj/main.rs"),
            contents: Some("fn main() {}\n".into()),
        });
        let events = agent.poll();
        assert!(
            matches!(
                events.as_slice(),
                [AgentEvent::ProposedEdit { .. }, AgentEvent::TurnEnded { .. }]
            ),
            "and resumes once answered, got {events:?}"
        );
    }

    #[test]
    fn what_the_client_served_is_recorded_for_assertions() {
        let mut agent = started();
        let session = session_of(&mut agent);
        agent.send(AgentRequest::FileContents {
            session,
            request: ReadRequestId::new(1),
            path: PathBuf::from("/proj/a.rs"),
            contents: Some("live buffer text".into()),
        });

        assert_eq!(agent.served().len(), 1);
        assert_eq!(agent.served()[0].1.as_deref(), Some("live buffer text"));
    }

    #[test]
    fn proposals_get_distinct_ids() {
        let mut agent = ScriptedAgent::new().with_turn(vec![
            ScriptedUpdate::Edit {
                path: PathBuf::from("/a"),
                old_text: None,
                new_text: "a".into(),
            },
            ScriptedUpdate::Edit {
                path: PathBuf::from("/b"),
                old_text: None,
                new_text: "b".into(),
            },
        ]);
        agent.send(AgentRequest::NewSession { cwd: PathBuf::from("/proj") });
        let session = session_of(&mut agent);

        let ids: Vec<ProposalId> = prompt(&mut agent, session)
            .iter()
            .filter_map(|e| match e {
                AgentEvent::ProposedEdit { proposal, .. } => Some(*proposal),
                _ => None,
            })
            .collect();
        assert_eq!(ids.len(), 2);
        assert_ne!(ids[0], ids[1]);
    }

    #[test]
    fn a_permission_request_carries_an_argv_array() {
        let mut agent = ScriptedAgent::new().with_turn(vec![ScriptedUpdate::Permission {
            summary: "run the tests".into(),
            command: vec!["cargo".into(), "test".into()],
        }]);
        agent.send(AgentRequest::NewSession { cwd: PathBuf::from("/proj") });
        let session = session_of(&mut agent);

        match prompt(&mut agent, session).as_slice() {
            [AgentEvent::PermissionRequested { command, .. }] => {
                assert_eq!(command, &["cargo", "test"], "argv, never a shell string");
            }
            other => panic!("expected a permission request, got {other:?}"),
        }
    }

    #[test]
    fn cancelling_drops_a_parked_turn() {
        let mut agent = ScriptedAgent::new().with_turn(vec![
            ScriptedUpdate::ReadFile(PathBuf::from("/proj/main.rs")),
            ScriptedUpdate::Message("should never arrive".into()),
        ]);
        agent.send(AgentRequest::NewSession { cwd: PathBuf::from("/proj") });
        let session = session_of(&mut agent);
        let _ = prompt(&mut agent, session);

        agent.send(AgentRequest::Cancel { session });
        assert!(matches!(
            agent.poll().as_slice(),
            [AgentEvent::TurnEnded { reason: StopReason::Cancelled, .. }]
        ));

        agent.send(AgentRequest::FileContents {
            session,
            request: ReadRequestId::new(1),
            path: PathBuf::from("/proj/main.rs"),
            contents: Some("x".into()),
        });
        assert!(agent.poll().is_empty(), "a cancelled turn does not resume");
    }

    #[test]
    fn prompting_without_a_session_fails_loudly_rather_than_hanging() {
        let mut agent = ScriptedAgent::new().with_turn(vec![ScriptedUpdate::End]);
        agent.send(AgentRequest::Prompt {
            session: SessionId::new(1),
            text: "hi".into(),
            context: String::new(),
        });
        assert!(matches!(
            agent.poll().as_slice(),
            [AgentEvent::Ready { .. }, AgentEvent::Failed { .. }]
        ));
    }

    #[test]
    fn an_exhausted_script_ends_turns_without_producing_anything() {
        let mut agent = started();
        let session = session_of(&mut agent);
        assert!(agent.is_exhausted());
        assert!(prompt(&mut agent, session).is_empty(), "no script, no events");
    }
}
