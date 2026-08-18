//! Smoke tests against a **real** ACP agent.
//!
//! Everything else in this crate proves the client is internally consistent; this proves
//! it talks to something that exists. `#[ignore]`d, because CI has no agent installed and
//! a test that silently skips is worse than one you have to ask for:
//!
//! ```text
//! TERMESH_TEST_AGENT="opencode acp" cargo test -p termesh-agent --test live_agent -- --ignored --nocapture
//! ```
//!
//! No prompt is sent, so no model is invoked and nothing is billed: this exercises the
//! handshake and session setup, which is where a protocol mismatch actually shows up.

use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

use termesh_agent::service::{AgentEvent, AgentRequest, AgentService, ClientCapabilities};
use termesh_agent::AcpAgent;

/// The agent command, as argv, from `TERMESH_TEST_AGENT`.
fn command() -> Option<Vec<String>> {
    let raw = std::env::var("TERMESH_TEST_AGENT").ok()?;
    let argv: Vec<String> = raw.split_whitespace().map(str::to_string).collect();
    if argv.is_empty() {
        return None;
    }
    Some(argv)
}

#[test]
#[ignore = "needs a real ACP agent; set TERMESH_TEST_AGENT"]
fn a_real_agent_completes_the_handshake_and_opens_a_session() {
    let Some(argv) = command() else {
        panic!("set TERMESH_TEST_AGENT, e.g. TERMESH_TEST_AGENT=\"opencode acp\"");
    };
    let cwd = std::env::temp_dir();

    let (tx, events) = mpsc::channel();
    let mut agent =
        AcpAgent::spawn(&argv, Path::new(&cwd), ClientCapabilities::default(), move |e| {
            let _ = tx.send(e);
        })
        .expect("the agent should spawn");

    // The handshake is implicit — `spawn` sends `initialize` — so asking for a session is
    // what proves it completed: the translator queues until the agent has replied.
    agent.send(AgentRequest::NewSession { cwd });

    let deadline = Duration::from_secs(30);
    loop {
        match events.recv_timeout(deadline) {
            Ok(AgentEvent::SessionStarted { session }) => {
                println!("session started: {session}");
                return;
            }
            // Authentication is the likely stopping point on a fresh machine, and it is
            // still a pass for what this test checks: the agent understood us well enough
            // to refuse for a reason.
            Ok(AgentEvent::Failed { message, .. }) => {
                println!("agent declined: {message}");
                assert!(
                    !message.contains("exited"),
                    "the agent died rather than answering: {message}"
                );
                return;
            }
            Ok(other) => println!("(ignoring {other:?})"),
            Err(e) => panic!("no answer from the agent within {deadline:?}: {e}"),
        }
    }
}

/// The reviewable-edit path, end to end against a real agent (ADR-0016).
///
/// Unlike the handshake test above, this **does** send a prompt, so it invokes a model and
/// may cost money. It is the only test that proves the whole chain — real agent, real
/// stdio, real translator — produces a permission carrying the change it wants to make,
/// which is the claim the feature rests on. Everything else asserts against payloads a
/// human copied out of a recording, and a copied payload cannot notice the day an agent
/// changes shape.
///
/// The agent must be one that asks before editing, and must be configured to:
///
/// ```text
/// # opencode, with {"permission": {"edit": "ask"}} in opencode.json
/// TERMESH_TEST_AGENT="opencode acp" TERMESH_TEST_EDIT=1 \
///   cargo test -p termesh-agent --test live_agent -- --ignored --nocapture
/// ```
#[test]
#[ignore = "sends a prompt to a real agent; set TERMESH_TEST_AGENT and TERMESH_TEST_EDIT"]
fn a_real_agent_asking_to_edit_hands_over_the_change() {
    let Some(argv) = command() else {
        panic!("set TERMESH_TEST_AGENT, e.g. TERMESH_TEST_AGENT=\"opencode acp\"");
    };
    if std::env::var("TERMESH_TEST_EDIT").is_err() {
        panic!("set TERMESH_TEST_EDIT=1 to confirm you want a billable prompt");
    }

    // A throwaway file, so the agent has something concrete to change and nothing of the
    // user's is at risk if it decides to edit before asking.
    let cwd = std::env::temp_dir().join("termesh-live-edit");
    std::fs::create_dir_all(&cwd).expect("scratch dir");
    let file = cwd.join("subject.rs");
    std::fs::write(&file, "fn vat() -> u32 {\n    1\n}\n").expect("scratch file");

    let (tx, events) = mpsc::channel();
    let mut agent = AcpAgent::spawn(
        &argv,
        &cwd,
        // Advertising the write capability is the point: an agent that would rather write
        // through us must be able to see that it can.
        ClientCapabilities { read_text_file: true, write_text_file: true, terminal: true },
        move |e| {
            let _ = tx.send(e);
        },
    )
    .expect("the agent should spawn");

    agent.send(AgentRequest::NewSession { cwd: cwd.clone() });

    let deadline = Duration::from_secs(45);
    let session = loop {
        match events.recv_timeout(deadline) {
            Ok(AgentEvent::SessionStarted { session }) => break session,
            Ok(AgentEvent::Failed { message, .. }) => panic!("no session: {message}"),
            Ok(other) => println!("(ignoring {other:?})"),
            Err(e) => panic!("no session within {deadline:?}: {e}"),
        }
    };

    agent.send(AgentRequest::Prompt {
        session,
        text: "In subject.rs, add the doc comment `/// VAT.` directly above `fn vat`. \
               Make the edit."
            .into(),
        context: String::new(),
    });

    let deadline = Duration::from_secs(300);
    loop {
        match events.recv_timeout(deadline) {
            Ok(AgentEvent::PermissionRequested { edit: Some(edit), .. }) => {
                println!("asked to edit {}", edit.path.display());
                assert!(
                    edit.new_text.contains("/// VAT."),
                    "the permission carries the change: {edit:?}"
                );
                // The gate is the point: it asked before writing.
                let on_disk = std::fs::read_to_string(&file).expect("subject");
                assert!(
                    !on_disk.contains("/// VAT."),
                    "the agent wrote before asking, so nothing was gated"
                );
                return;
            }
            Ok(AgentEvent::TurnEnded { .. }) => {
                let on_disk = std::fs::read_to_string(&file).unwrap_or_default();
                panic!(
                    "the turn ended without asking to edit. On disk now:\n{on_disk}\n\
                     If the file changed, this agent writes without asking — see \
                     docs/support.md for which agents gate edits and how to configure them."
                );
            }
            Ok(other) => println!("(ignoring {other:?})"),
            Err(e) => panic!("no permission within {deadline:?}: {e}"),
        }
    }
}
