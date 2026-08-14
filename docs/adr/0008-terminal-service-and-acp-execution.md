# 8. One terminal service for human shells and ACP command execution

Date: 2026-08-09

## Status
Accepted

## Context

Phase 04 must deliver interactive terminal tabs and the command-execution half of the agent
loop. Its exit criterion is deliberately one sentence because the two features are one system:
interactive shells, builds, and tests work cross-platform, and an agent can run an approved
command and read its output.

Three accepted decisions constrain the design:

- ADR-0002 selected `portable-pty` for PTYs and `alacritty_terminal` for terminal emulation.
- ADR-0005 made synchronous service traits on worker threads the default for OS-facing services.
  A different concurrency model needs an ADR.
- ADR-0007 made permission requests prompt-always by default, required exact argv display, and
  deferred actual command execution to Phase 04.

ACP v1 already defines the client-side execution surface we need. An agent calls
`terminal/create`, then `terminal/output`, `terminal/wait_for_exit`, `terminal/kill`, and
`terminal/release`. A terminal ID may be embedded in a tool call so the client can display its
live output. The client advertises this support as `clientCapabilities.terminal = true`.
Inventing a second command-result protocol would be both incompatible and unnecessary.

The current application has a placeholder `PtyService`, an existing `TerminalId`, and registry
entries for `terminal.new` and `terminal.run`. It has no terminal state, PTY worker, terminal
input context, output capture, process lifecycle, clipboard implementation, or ACP `terminal/*`
translation. The current permission UI answers the agent immediately but cannot yet enforce that
a later command is the command the human approved.

This phase therefore touches two load-bearing surfaces named in CLAUDE.md: the action registry
and ACP semantics. This ADR resolves them before implementation.

## Options considered

### A. One PTY service, worker threads, and one terminal model

Human shells, Tier 0 AI CLIs, user-launched commands, and ACP-managed commands all become terminal
sessions. A worker owns OS PTY handles and sends bounded byte chunks and lifecycle events to the
application message bus. The single-owner model feeds those bytes through
`alacritty_terminal`, owns the resulting grids, and renders them. ACP commands add permission,
correlation, and bounded output capture around the same session type.

This follows ADR-0005, keeps the view pure, and makes the fake exercise the production path.

### B. Introduce Tokio actors for PTY I/O

One async task per PTY plus an async application loop is a reasonable long-term shape. It is not
needed for a handful of blocking PTY streams, would replace the already-working channel loop,
and would force the filesystem and ACP workers into a runtime migration during a terminal
feature phase. Rejected as premature. Tokio remains available as a later migration whose service
boundary need not change.

### C. Separate interactive terminals from agent command runners

Interactive shells could use a terminal emulator while agent commands use redirected child
process pipes. This is less code for a first command, but it creates two implementations of
spawn, cancellation, exit status, output bounding, working-directory handling, and rendering.
It also breaks ARCHITECTURE.md §11's requirement that agent execution use the managed terminal
path. Rejected.

## Decision

We take Option A.

### 1. The model owns terminal state; the service owns only OS resources

`PtyService` remains synchronous and object-safe. It is called only by `PtyWorker`, never by a
widget, the renderer, or the model. The real implementation uses `portable-pty`; test-support
provides a scripted fake with the same request/event path.

The protocol-neutral types live in `core`:

```rust
pub struct TerminalSpec {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub env: Vec<(String, String)>,
}

pub enum TerminalOwner {
    HumanShell,
    HumanCommand,
    Agent { session: SessionId },
}

pub enum PtyRequest {
    Spawn { terminal: TerminalId, spec: TerminalSpec, size: TerminalSize },
    Write { terminal: TerminalId, bytes: Vec<u8> },
    Resize { terminal: TerminalId, size: TerminalSize },
    Kill { terminal: TerminalId },
    Release { terminal: TerminalId },
}

pub enum PtyEvent {
    Spawned { terminal: TerminalId, process_id: Option<u32> },
    Output { terminal: TerminalId, bytes: Vec<u8> },
    Exited { terminal: TerminalId, exit: TerminalExit },
    Failed { terminal: TerminalId, message: String },
}
```

The exact Rust layout may split these declarations across focused files, but these fields and
responsibilities are the contract. Output events are chunked at 32 KiB so a noisy build cannot
monopolize one model update. The model owns one `TerminalScreen` per retained session; feeding a
chunk through the VT parser is a state update, not I/O. Parser responses that must be written back
to the PTY are queued as ordinary `PtyRequest::Write` effects.

### 2. Terminal sessions are unified but retain their origin

Every tab has a typed `TerminalId`, an immutable launch `TerminalSpec`, a `TerminalOwner`, an
`alacritty_terminal` screen, a status (`Starting | Running | Exited | Failed`), and bounded
captured output. Origin is metadata, not a different implementation:

- `HumanShell` starts the platform's default interactive shell in the workspace root.
- `HumanCommand` is launched by the existing `terminal.run` action. The action prompts for a
  human-authored command line and passes it to the platform shell (`-lc` on Unix, `/D /S /C` on
  Windows). This exception is safe because the human authored the shell text; agent-controlled
  commands always use structured program and argv fields.
- `Agent` is created by ACP `terminal/create` and is labelled in the tab and agent tool call.

Restart creates a fresh process for the same tab and spec after the old process has exited or
been killed. Closing a running terminal asks for confirmation, kills it, then releases its OS
resources. Releasing an ACP terminal frees PTY resources but retains its final screen and captured
output while a tool call still references it, as ACP recommends.

Terminal scrollback is capped at 10,000 lines by default. ACP capture is independently capped by
the request's `outputByteLimit`, with a 1 MiB default and an 8 MiB client ceiling. Truncation drops
the oldest complete UTF-8 prefix and records `truncated = true`. Invalid process bytes are
rendered and reported with replacement characters; raw bytes still feed the VT parser.

### 3. Input is shell-first while the terminal has focus

`KeyContext` gains `Terminal`. In normal terminal mode, all keyboard input is encoded for the
active PTY, including Tab, Ctrl+C, arrows, function keys, Alt sequences, and application-cursor
mode. IDE-global shortcuts do not shadow shell input.

The **unconditionally** reserved chords are the **pane-focus actions**: `F6` (`terminal.focus`),
plus `F1`/`F2`/`F7` (`focus.project` / `focus.editor` / `focus.agent`). `F6` specifically:

- from another pane, it focuses the active terminal and lazily creates the first shell if needed;
- from the terminal, it restores the most recent non-terminal pane.

This is the reliable escape hatch. Users can then invoke the ordinary palette and global actions.
No second prefix state is introduced.

**The `Tab` focus ring excludes the Terminal.** Originally it did not, and that was a dead end: the
shell owns `Tab`, so the pane could be entered by cycling but never left by it. `F6` returned the
user to the pane they came from, the next `Tab` put them straight back, and the Project and Agent
panes became unreachable in the forward direction. A pane that captures the cycle key cannot be a
member of the cycle. It is reached by its own chord instead, the way an IDE panel is.

More than one chord is reserved for the same reason: the way out must not be a single key. Every
pane is directly reachable from inside a running shell, so the user is never one forgotten chord
away from being stranded.

These are **function keys, deliberately**. A reserved chord must survive both a shell and whatever
Tier 0 is hosting, and between them they claim nearly every `Ctrl`+letter — Claude Code alone binds
`Ctrl`+A–E, G, J–L, N–P, and R–X, including `Ctrl+T`, which was briefly tried here and withdrawn.
`Ctrl+\`` — the IDE convention — is not reachable by a TUI at all: in the legacy encoding it would
have to arrive as NUL (`0x60 & 0x1f`), most emulators send nothing for it, and NUL is
indistinguishable from `Ctrl+Space`, which macOS claims for input-source switching.
`KeyChord::is_terminal_ambiguous` rejects the whole NUL family so no default binding can reach for
it again.

Because a focused shell swallows everything else, the escapes are resolved *through the keymap*
rather than hardcoded: `Command::is_pane_focus` names the set, and rebinding any of them moves the
escape with it, so a user cannot rebind themselves into a shell they cannot leave.

Beyond that always-on set, a chord may be reserved **only while a specific state holds**, and only
where handing it to the shell would be the wrong answer for what is on screen. Two exist:

- **copy mode** — entered deliberately through the action registry, during which the terminal's
  own navigation chords resolve instead of reaching the PTY;
- **`task.cancel` while the focused terminal is running a task** (ADR-0009 §4) — the process the
  user is watching is one the *IDE* started and owns, so cancelling it should not require leaving
  the pane first. When no task is running there, the chord goes to the shell like any other.

The distinction that matters: a state-scoped exception must be narrow, observable to the user, and
never the *only* way out of the pane. Adding one is an ADR-level change, not a local decision.

Copy is an explicit terminal copy mode entered through the action registry. It uses
`alacritty_terminal`'s scrollback and selection state: arrows move, Shift+arrows extend, Enter
copies, and Esc cancels. Clipboard output goes through `ClipboardService`; the real terminal-native
implementation uses an OSC 52 write so copying works through SSH, and test-support supplies a fake.
Agent-controlled output can never trigger clipboard writes: OSC 52 received from the child is not
honoured, and only a human copy action calls `ClipboardService`.

### 4. Terminal behavior remains on the shared command surface

The existing `terminal.new` and `terminal.run` actions stay stable. The registry gains:

```text
terminal.focus
terminal.next
terminal.previous
terminal.restart
terminal.close
terminal.copy_mode
```

Their commands are used by the palette and keymap; the view does not special-case behavior.
Literal PTY input is the terminal equivalent of editor character insertion: data, not a finite
registry action. `Ctrl+\`` dispatches `terminal.focus`; the action itself decides whether that
means enter or leave based on current focus.

### 5. ACP uses its standard terminal methods and is permission-enforced by the client

The client advertises terminal support only after the whole surface is wired. `agent` translates
ACP wire requests into protocol-neutral events and replies supplied by the model:

```text
terminal/create         -> authorize -> PtyRequest::Spawn -> terminalId response
terminal/output         -> captured output + current exit status
terminal/wait_for_exit  -> held response completed by PtyEvent::Exited
terminal/kill           -> PtyRequest::Kill, terminal remains queryable
terminal/release        -> kill if needed, release resources, invalidate wire ID
```

Wire terminal IDs remain private to `agent`. A correlation ID joins an in-flight ACP request to a
typed core `TerminalId`; neither ACP strings nor schema types leak into `terminal`, `app`, or `ui`.

Permission is enforced even though ACP says an agent may choose whether to call
`session/request_permission`:

1. An explicit allow for an execute tool call records a one-shot grant only when its structured
   program and argv can be matched exactly.
2. A matching `terminal/create` consumes that grant and starts without a second prompt.
3. A create without a matching grant is held and surfaced through the same permission UI.
4. Rejecting returns a JSON-RPC error and starts no process.
5. Shell strings are never split or reinterpreted to manufacture a match. An ambiguous grant may
   cause another prompt; it never causes an unapproved launch.
6. A one-shot grant is scoped to the turn it was given in. If the agent does not spend it before
   that turn ends or is cancelled, it expires. Unbounded grants would violate (5): an approval the
   user gave twenty turns ago would silently preauthorize an identical create they never saw.

The prompt shows program, each argv element, cwd, and agent-supplied environment overrides. The
worker passes them to `portable-pty::CommandBuilder` as separate fields. Agent text is never
interpolated into a shell command.

`AllowAlways` is scoped to the current workspace and an exact program+argv signature. It applies
only when cwd is inside the workspace and there are no agent-supplied environment overrides.
Those grants are stored under `[agent.permissions]` in `.termesh/workspace.toml`; `toml_edit` is
used so recording a grant preserves unrelated keys and comments. Requests with env overrides or
an out-of-workspace cwd remain allow-once/reject-only.

The stored shape is an array of exact command entries. `cwd` is workspace-relative and `"."`
means the root:

```toml
[[agent.permissions.commands]]
program = "cargo"
args = ["test", "--workspace"]
cwd = "."
```

### 6. Failures are visible state, not loop failures

A missing shell, failed PTY allocation, write error, or process crash marks only that terminal as
failed/exited and leaves the IDE running. Unknown terminal IDs and malformed ACP terminal requests
receive deterministic JSON-RPC errors. A `wait_for_exit` request is completed on normal exit,
kill, spawn failure, release, agent cancellation, or shutdown so the agent is never left blocked.

On application shutdown, every live PTY is released. Closing the outer TUI still restores raw mode
and the alternate screen through the existing panic-safe path.

## Consequences

**No Tokio yet.** PTYs become another producer on the existing message bus. The worker-thread
template remains consistent across filesystem, ACP, and terminal services.

**Dependencies.** `portable-pty` becomes active and moves to its current compatible 0.9 line.
`alacritty_terminal` 0.26 provides the VT parser/grid. `base64` 0.22 is used for OSC 52 clipboard
payloads, and `toml_edit` 0.22 preserves the workspace permission file. All four are isolated to
the crates that use them. ADR-0002 already chose the two heavy terminal dependencies; this ADR
accepts the two small support dependencies for the concrete Phase-04 behavior.

**The agent boundary grows, but wire types stay isolated.** `AgentEvent`/`AgentRequest` gain
protocol-neutral terminal effects and replies. All method names, wire terminal strings, JSON
shapes, and schema interpretation remain in `crates/agent/src/protocol.rs`.

**Fakes are mandatory.** `test-support` gains `ScriptedPty` and `FakeClipboard`. ANSI parsing,
tabs, input routing, resize, copy mode, output truncation, permissions, ACP request correlation,
and exit handling are all deterministic without a shell or OS PTY. A separate real-PTY smoke test
spawns the current test executable as a helper so the portable-pty wiring is exercised without
assuming Bash or PowerShell.

**Headless proof grows.** `termesh --dump-frame --terminal-demo` feeds a recorded ANSI stream into
the same terminal model and renders tabs, colors, cursor, and process state under Ratatui's
`TestBackend`.

**Deferred deliberately.** Shell configuration UI, terminal search, mouse selection, hyperlinks,
split terminals, task/problem matching, and session restoration of live processes are not Phase
04. Phase 05 consumes the managed-command path for tasks rather than inventing another runner.
