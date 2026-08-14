//! Smoke tests against a **real** ACP agent.
//!
//! Everything else in this crate proves the client is internally consistent; this proves
//! it talks to something that exists. `#[ignore]`d, because CI has no agent installed and
//! a test that silently skips is worse than one you have to ask for:
//!
//! ```text
//! TERMIDE_TEST_AGENT="opencode acp" cargo test -p termesh-agent --test live_agent -- --ignored --nocapture
//! ```
//!
//! No prompt is sent, so no model is invoked and nothing is billed: this exercises the
//! handshake and session setup, which is where a protocol mismatch actually shows up.

use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

use termesh_agent::service::{AgentEvent, AgentRequest, AgentService, ClientCapabilities};
use termesh_agent::AcpAgent;

/// The agent command, as argv, from `TERMIDE_TEST_AGENT`.
fn command() -> Option<Vec<String>> {
    let raw = std::env::var("TERMIDE_TEST_AGENT").ok()?;
    let argv: Vec<String> = raw.split_whitespace().map(str::to_string).collect();
    if argv.is_empty() {
        return None;
    }
    Some(argv)
}

#[test]
#[ignore = "needs a real ACP agent; set TERMIDE_TEST_AGENT"]
fn a_real_agent_completes_the_handshake_and_opens_a_session() {
    let Some(argv) = command() else {
        panic!("set TERMIDE_TEST_AGENT, e.g. TERMIDE_TEST_AGENT=\"opencode acp\"");
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
