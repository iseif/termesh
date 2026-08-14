//! Protocol-neutral terminal vocabulary shared across the application boundary.
//!
//! OS handles stay in `terminal` and ACP wire identifiers stay in `agent`; these types
//! are the typed effects joining those services to the single-owner model (ADR-0008).

use std::path::PathBuf;

use crate::{SessionId, TerminalGeneration, TerminalId};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalSpec {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub env: Vec<(String, String)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalSize {
    pub rows: u16,
    pub cols: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalOwner {
    HumanShell,
    HumanCommand,
    Agent { session: SessionId },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalStatus {
    Starting,
    Running { process_id: Option<u32> },
    Exited(TerminalExit),
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalExit {
    pub code: Option<u32>,
    pub signal: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PtyRequest {
    Spawn {
        terminal: TerminalId,
        generation: TerminalGeneration,
        spec: TerminalSpec,
        size: TerminalSize,
    },
    Write {
        terminal: TerminalId,
        generation: TerminalGeneration,
        bytes: Vec<u8>,
    },
    Resize {
        terminal: TerminalId,
        generation: TerminalGeneration,
        size: TerminalSize,
    },
    Kill {
        terminal: TerminalId,
        generation: TerminalGeneration,
    },
    Release {
        terminal: TerminalId,
        generation: TerminalGeneration,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PtyEvent {
    Spawned { terminal: TerminalId, generation: TerminalGeneration, process_id: Option<u32> },
    Output { terminal: TerminalId, generation: TerminalGeneration, bytes: Vec<u8> },
    Exited { terminal: TerminalId, generation: TerminalGeneration, exit: TerminalExit },
    Failed { terminal: TerminalId, generation: TerminalGeneration, message: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentTerminalOperation {
    Create { spec: TerminalSpec, output_byte_limit: usize, preauthorized: bool },
    Output { terminal: TerminalId },
    WaitForExit { terminal: TerminalId },
    Kill { terminal: TerminalId },
    Release { terminal: TerminalId },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentTerminalResponse {
    Created { terminal: TerminalId },
    Output { output: String, truncated: bool, exit: Option<TerminalExit> },
    Exited(TerminalExit),
    Acknowledged,
    Error(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn terminal_spec_keeps_program_and_arguments_separate() {
        let spec = TerminalSpec {
            program: "cargo".into(),
            args: vec!["test".into(), "--workspace".into()],
            cwd: PathBuf::from("/proj"),
            env: vec![("RUST_BACKTRACE".into(), "1".into())],
        };

        assert_eq!(spec.program, "cargo");
        assert_eq!(spec.args, ["test", "--workspace"]);
    }
}
