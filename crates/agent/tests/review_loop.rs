//! The review loop, end to end against a scripted agent.
//!
//! ADR-0007 §5 rests on one assumption: because *we* serve `fs/read_text_file`, we know
//! exactly what the agent read and can anchor its proposal to that text. Everything about
//! safe diff-review follows from it, so it is asserted here rather than trusted — through
//! the real `Buffer`, the real diff machinery, and a scripted agent standing in for the
//! subprocess.
//!
//! These are cross-crate on purpose. The unit tests in `agent` and `editor` each check
//! their own half; this checks that the halves agree.

use std::path::{Path, PathBuf};

use termesh_agent::service::{AgentEvent, AgentRequest, AgentService};
use termesh_agent::{changeset_from_hunks, hunks_from_diff, rebase_hunks, Hunk};
use termesh_core::{BufferId, ProposalId};
use termesh_editor::{Buffer, EditSource, HunkState, Selection, Version};
use termesh_test_support::{ScriptedAgent, ScriptedUpdate};

const PATH: &str = "/proj/main.rs";
const ORIGINAL: &str = "fn main() {\n    println!(\"hi\");\n}\n";
const PROPOSED: &str = "fn run() {\n    println!(\"hi\");\n}\n";

fn buffer(text: &str) -> Buffer {
    Buffer::from_text(BufferId::new(1), Some(PathBuf::from(PATH)), text)
}

/// What we served the agent and when — the read-set of ADR-0007 §5.
#[derive(Default)]
struct ReadSet {
    entries: Vec<(PathBuf, Version, String)>,
}

impl ReadSet {
    fn record(&mut self, path: &Path, version: Version, text: &str) {
        self.entries.push((path.to_path_buf(), version, text.to_string()));
    }

    /// The version whose served text matches `old_text`, if any.
    ///
    /// A match means the proposal is anchored to a revision we hold — the clean path.
    /// A miss means the agent read the file some other way, and we fall back to
    /// anchoring by content.
    fn anchor(&self, path: &Path, old_text: &str) -> Option<Version> {
        self.entries
            .iter()
            .rev()
            .find(|(p, _, text)| p == path && text == old_text)
            .map(|(_, version, _)| *version)
    }

    fn served_text(&self, path: &Path) -> Option<&str> {
        self.entries.iter().rev().find(|(p, ..)| p == path).map(|(_, _, t)| t.as_str())
    }
}

/// Drive a scripted agent through one turn, serving reads from `buffer` — which is the
/// whole point: the agent sees unsaved editor state, not the disk.
fn run_turn(agent: &mut ScriptedAgent, buffer: &Buffer, reads: &mut ReadSet) -> Vec<AgentEvent> {
    agent.send(AgentRequest::NewSession { cwd: PathBuf::from("/proj") });
    let startup = agent.poll();
    let session = startup
        .iter()
        .find_map(|event| match event {
            AgentEvent::SessionStarted { session } => Some(*session),
            _ => None,
        })
        .unwrap_or_else(|| panic!("expected a session, got {startup:?}"));

    agent.send(AgentRequest::Prompt {
        session,
        text: "rename main to run".into(),
        context: "project: proj (rust)".into(),
    });

    let mut collected = Vec::new();
    loop {
        let events = agent.poll();
        if events.is_empty() {
            return collected;
        }
        for event in events {
            if let AgentEvent::ReadFileRequested { request, path, .. } = &event {
                let text = buffer.text().to_string();
                reads.record(path, buffer.version(), &text);
                agent.send(AgentRequest::FileContents {
                    session,
                    request: *request,
                    path: path.clone(),
                    contents: Some(text),
                });
            }
            collected.push(event);
        }
    }
}

fn script() -> ScriptedAgent {
    ScriptedAgent::new().with_turn(vec![
        ScriptedUpdate::Message("Renaming `main` to `run`.".into()),
        ScriptedUpdate::ReadFile(PathBuf::from(PATH)),
        ScriptedUpdate::Edit {
            path: PathBuf::from(PATH),
            old_text: Some(ORIGINAL.into()),
            new_text: PROPOSED.into(),
        },
        ScriptedUpdate::End,
    ])
}

/// The proposal from a turn, as `(old_text, new_text)`.
fn proposed(events: &[AgentEvent]) -> (ProposalId, String, String) {
    events
        .iter()
        .find_map(|e| match e {
            AgentEvent::ProposedEdit { proposal, old_text, new_text, .. } => {
                Some((*proposal, old_text.clone().unwrap_or_default(), new_text.clone()))
            }
            _ => None,
        })
        .expect("the turn should have proposed an edit")
}

/// Accept every applicable hunk as one transaction — one undo step (ADR-0006 §6).
fn accept(buffer: &mut Buffer, proposal: ProposalId, hunks: &[Hunk]) {
    let applicable: Vec<&Hunk> = hunks.iter().filter(|h| h.state.is_applicable()).collect();
    if applicable.is_empty() {
        return;
    }
    let changes = changeset_from_hunks(&applicable, buffer.text().len_chars());
    let tx = buffer.transaction(changes, EditSource::Agent(proposal));
    buffer.apply(&tx).expect("an accepted proposal should apply");
}

// -----------------------------------------------------------------------------------

/// The agent reads through us, so `old_text` is the buffer — including unsaved edits.
#[test]
fn the_agent_reads_the_live_buffer_not_the_disk() {
    let mut buffer = buffer(ORIGINAL);
    // An unsaved change the disk knows nothing about.
    buffer.set_selection(Selection::point(0));
    buffer.insert("// unsaved\n", EditSource::Keyboard).unwrap();

    let mut agent = script();
    let mut reads = ReadSet::default();
    run_turn(&mut agent, &buffer, &mut reads);

    let served = agent.served().first().expect("the agent asked for the file");
    assert_eq!(
        served.1.as_deref(),
        Some(buffer.text().to_string().as_str()),
        "the agent must see what the human sees, unsaved and all"
    );
    assert!(served.1.as_ref().unwrap().starts_with("// unsaved"));
}

/// The clean path: nothing changed under the proposal, so it anchors to a real revision.
#[test]
fn a_proposal_anchors_to_the_version_we_served() {
    let mut buffer = buffer(ORIGINAL);
    let mut agent = script();
    let mut reads = ReadSet::default();

    let events = run_turn(&mut agent, &buffer, &mut reads);
    let (proposal, old_text, new_text) = proposed(&events);

    let anchor = reads.anchor(Path::new(PATH), &old_text);
    assert_eq!(anchor, Some(buffer.version()), "anchored to the revision we served");

    let hunks = hunks_from_diff(&old_text, &new_text);
    accept(&mut buffer, proposal, &hunks);
    assert_eq!(buffer.text().to_string(), PROPOSED);
}

/// The whole reason the spine exists: the human keeps typing while the agent thinks.
#[test]
fn a_proposal_still_applies_after_the_human_edits_elsewhere() {
    let mut buffer = buffer(ORIGINAL);
    let mut agent = script();
    let mut reads = ReadSet::default();
    let events = run_turn(&mut agent, &buffer, &mut reads);
    let (proposal, old_text, new_text) = proposed(&events);

    // ... and only now does the human type, above the proposed change.
    buffer.set_selection(Selection::point(0));
    buffer.insert("// note\n", EditSource::Keyboard).unwrap();

    assert_ne!(reads.anchor(Path::new(PATH), &old_text), Some(buffer.version()), "moved on");

    let mut hunks = hunks_from_diff(&old_text, &new_text);
    rebase_hunks(&mut hunks, &old_text, &buffer.text().to_string());
    assert!(hunks.iter().all(|h| h.state == HunkState::Clean));

    accept(&mut buffer, proposal, &hunks);
    assert_eq!(buffer.text().to_string(), format!("// note\n{PROPOSED}"));
}

/// ADR-0006 §4 case 2, through the whole stack: never silently overwrite the human.
#[test]
fn a_proposal_conflicts_when_the_human_edits_the_same_line() {
    let mut buffer = buffer(ORIGINAL);
    let mut agent = script();
    let mut reads = ReadSet::default();
    let events = run_turn(&mut agent, &buffer, &mut reads);
    let (proposal, old_text, new_text) = proposed(&events);

    // The human renames it themselves, differently.
    let target = ORIGINAL.find("main").unwrap();
    buffer.edit(target, target + 4, "start", EditSource::Keyboard).unwrap();
    let after_human = buffer.text().to_string();

    let mut hunks = hunks_from_diff(&old_text, &new_text);
    rebase_hunks(&mut hunks, &old_text, &after_human);

    assert!(
        hunks.iter().any(|h| matches!(h.state, HunkState::Conflicted(_))),
        "got {:?}",
        hunks.iter().map(|h| h.state).collect::<Vec<_>>()
    );

    accept(&mut buffer, proposal, &hunks);
    assert_eq!(buffer.text().to_string(), after_human, "the human's edit survives untouched");
}

/// ADR-0006 §4 case 5 — the human beat the agent to it.
#[test]
fn a_proposal_settles_when_the_human_already_made_the_change() {
    let mut buffer = buffer(ORIGINAL);
    let mut agent = script();
    let mut reads = ReadSet::default();
    let events = run_turn(&mut agent, &buffer, &mut reads);
    let (proposal, old_text, new_text) = proposed(&events);

    let target = ORIGINAL.find("main").unwrap();
    buffer.edit(target, target + 4, "run", EditSource::Keyboard).unwrap();
    let after_human = buffer.text().to_string();
    assert_eq!(after_human, PROPOSED, "the human made exactly the agent's change");

    let mut hunks = hunks_from_diff(&old_text, &new_text);
    rebase_hunks(&mut hunks, &old_text, &after_human);

    assert!(hunks.iter().all(|h| h.state == HunkState::Satisfied));
    accept(&mut buffer, proposal, &hunks);
    assert_eq!(buffer.text().to_string(), PROPOSED, "and nothing is applied twice");
}

/// The fallback ADR-0007 §5 promises: the agent read the file some other way, so
/// `old_text` matches no revision of ours. Anchoring by content still has to work.
#[test]
fn a_proposal_we_never_served_anchors_by_content() {
    let mut buffer = buffer(ORIGINAL);
    let mut reads = ReadSet::default();

    // No read request at all — the agent used its own tools.
    let mut agent = ScriptedAgent::new().with_turn(vec![ScriptedUpdate::Edit {
        path: PathBuf::from(PATH),
        old_text: Some(ORIGINAL.into()),
        new_text: PROPOSED.into(),
    }]);
    let events = run_turn(&mut agent, &buffer, &mut reads);
    let (proposal, old_text, new_text) = proposed(&events);

    assert!(reads.served_text(Path::new(PATH)).is_none(), "we served nothing");
    assert_eq!(reads.anchor(Path::new(PATH), &old_text), None, "so there is no version");

    let mut hunks = hunks_from_diff(&old_text, &new_text);
    rebase_hunks(&mut hunks, &old_text, &buffer.text().to_string());
    accept(&mut buffer, proposal, &hunks);

    assert_eq!(buffer.text().to_string(), PROPOSED, "content anchoring carries it");
}

/// The phase's exit criterion, in one test: propose, review, accept, undo.
#[test]
fn accept_then_undo_reverses_the_whole_proposal() {
    let mut buffer = buffer(ORIGINAL);
    let mut agent = script();
    let mut reads = ReadSet::default();
    let events = run_turn(&mut agent, &buffer, &mut reads);
    let (proposal, old_text, new_text) = proposed(&events);

    let hunks = hunks_from_diff(&old_text, &new_text);
    accept(&mut buffer, proposal, &hunks);
    assert_eq!(buffer.text().to_string(), PROPOSED);

    assert!(buffer.undo(), "one keystroke");
    assert_eq!(buffer.text().to_string(), ORIGINAL, "the agent's change, undone whole");
    assert!(!buffer.can_undo(), "and it was a single step");
}

/// Per-hunk review: take one change, leave the other.
#[test]
fn a_partial_accept_applies_only_what_was_taken() {
    let original = "one\ntwo\nthree\nfour\n";
    let mut buffer = buffer(original);
    let mut reads = ReadSet::default();
    let mut agent = ScriptedAgent::new().with_turn(vec![
        ScriptedUpdate::ReadFile(PathBuf::from(PATH)),
        ScriptedUpdate::Edit {
            path: PathBuf::from(PATH),
            old_text: Some(original.into()),
            new_text: "ONE\ntwo\nthree\nFOUR\n".into(),
        },
        ScriptedUpdate::End,
    ]);

    let events = run_turn(&mut agent, &buffer, &mut reads);
    let (proposal, old_text, new_text) = proposed(&events);

    let mut hunks = hunks_from_diff(&old_text, &new_text);
    assert_eq!(hunks.len(), 2, "two independent decisions");

    // The human rejects the second hunk.
    hunks[1].state = HunkState::Conflicted(termesh_editor::ConflictReason::EditedInsideRange);
    accept(&mut buffer, proposal, &hunks);

    assert_eq!(buffer.text().to_string(), "ONE\ntwo\nthree\nfour\n");
}
