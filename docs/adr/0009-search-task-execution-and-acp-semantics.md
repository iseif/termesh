# 9. Search and task execution use workers, managed terminals, and stable ACP

Date: 2026-08-09

## Status
Accepted

## Context

Phase 05 must deliver quick-open, ripgrep-backed workspace search, project-type awareness, a Cargo
task adapter, task output and cancellation, basic problem matching, and a permissioned agent path
to `task.run`. The exit criterion is one workflow: open a project, search it, run tests, and jump
to failures as either a human or agent.

Accepted decisions constrain the implementation:

- ADR-0005 makes synchronous service traits on worker threads the standard OS boundary and
  requires search to agree with the explorer's ignore behavior.
- ADR-0007 isolates ACP wire types in `agent` and makes the editor-owned workspace snapshot the
  agent's small per-turn context.
- ADR-0008 unifies human and agent command execution in one managed PTY service and explicitly
  says Phase 05 must consume that path rather than invent another runner.
- CLAUDE.md requires action-registry and ACP-semantic decisions to be recorded before
  implementation.

The `search` and `tasks` crates currently contain placeholder traits. `ProjectKind` already
detects Rust, Node, Python, and Go workspaces, while `workspace.search` and `task.run` already exist
as placeholder actions. The application has no search worker, search results, task catalog, task
run state, problem model, task cancellation, or result-navigation UI.

There is also a protocol constraint that the architecture's shorthand does not spell out. Stable
ACP v1 lets a client provide filesystem and terminal capabilities and lets a session declare MCP
servers. It does not define direct registration of arbitrary client-owned tools. A client can
provide an MCP stdio server, but a Termesh sidecar would need authenticated cross-process IPC back
to the live model. MCP-over-ACP exists only as an unstable optional capability and is not portable
across agents.

## Options considered

### A. Worker-backed search, adapter-backed tasks, and the existing ACP terminal path

Run `rg` behind `SearchService` on a worker. Let a language-neutral `TaskService` return curated
task specifications from adapters, with Cargo as the only production adapter in this phase. Run
every selected task through the existing PTY service and attach task/problem state to that managed
terminal.

Give the agent the same catalog in workspace context. Classify an exact ACP `terminal/create`
matching a catalog entry as the corresponding task after normal permission enforcement. This
delivers the same human and agent behavior entirely on stable ACP, although `task.run` is an
internal action rather than a literal custom wire method.

### B. Expose a literal `task.run` through an MCP stdio sidecar

Start the current binary in an MCP-server mode and pass that server to the ACP agent at
`session/new`. Because the agent owns the sidecar's stdio, the sidecar must connect back to the
running Termesh instance through a local socket or named pipe. This requires authentication,
multi-instance routing, cross-platform IPC, lifecycle recovery, and another JSON-RPC server before
one task can be run.

This makes the wire tool name exact but duplicates substantial infrastructure unrelated to the
Phase-05 user workflow. Rejected as premature.

### C. Use unstable MCP-over-ACP

Advertise an in-process MCP server over the existing ACP channel and implement `task.run` there.
This avoids a sidecar but depends on an explicitly unstable schema feature and agent capability.
Agents without the extension would receive a different product surface. Rejected because ACP
portability is a core product promise.

### D. Give tasks their own process runner

Spawn Cargo with pipes from `TaskService`, separate from interactive and agent terminals. This
would simplify structured stdout parsing but duplicate spawn, cwd, output bounding, cancellation,
exit state, permission, and rendering behavior already solved in Phase 04. Rejected by ADR-0008.

## Decision

We take Option A.

### 1. Search uses a service worker and structured ripgrep output

`SearchService` remains synchronous and object-safe and is called only by `SearchWorker`. The real
service starts `rg` with structured program and argv fields, never a shell string. Quick-open uses
`rg --files`; content search uses JSON output with fixed-string smart-case matching. The worker
streams bounded typed batches into `AppMessage::Search` and owns child cancellation and reaping.
If `rg` is unavailable, the same worker uses a native gitignore-aware walk for files and
fixed-string smart-case matching for text, preserving the user-facing search contract.

The two paths are **one user-facing contract**, not a fast path and a degraded one. Both report one
result per *occurrence*, so a line with three hits is three results either way: ripgrep reports a
line once and lists its hits under `submatches`, and every one of them is emitted. Both skip walk
entries the OS or an ignore file made unreadable rather than failing the search — ripgrep does, and
the fallback runs precisely on machines that have no ripgrep to fall back from, so it cannot be the
more brittle of the two. A test pins the two implementations against each other on the same input.

Every request has a typed `SearchRequestId`. The model ignores events for an older id, so killing
a process and starting a replacement cannot flash stale results into the new query. Open buffers
replace disk matches for their path using the same literal smart-case semantics, preserving the
editor as the source of truth for unsaved text.

*Every* open buffer is scanned, including clean ones, and synchronously: a clean buffer's file may
lie outside the workspace root or under an ignore rule where the worker's walk never reaches it, so
restricting this to dirty buffers would drop results rather than defer them. This is the immediate
half of search, which is why the debounce lives in the worker instead. Should the per-keystroke cost
ever matter, the answer is an incremental matcher, not a narrower set of buffers.

### 2. Tasks are adapter-backed but Cargo-only in Phase 05

`TaskService` exposes protocol-neutral `TaskSpec` values selected by `ProjectKind`. The adapter
boundary owns project applicability, curated commands, and task-output interpretation. Phase 05
ships only `CargoTaskAdapter`, with exact Check, Build, Test, and Clippy tasks. Target discovery,
user-defined tasks, and additional language/build adapters remain deferred.

This split is a product boundary, not a Rust-specific UI: search works in every project, and all
future task adapters reuse the same picker, execution, cancellation, output, problems, and agent
integration.

### 3. Tasks reuse managed PTYs

Selecting `task.run` allocates a `TaskRunId` and a normal managed terminal. The model joins the run
to its `TerminalId`; `TaskService` does not spawn Cargo. Cancellation is `PtyRequest::Kill`, and
the retained terminal remains available after exit or cancellation.

Cargo tasks request `json-diagnostic-rendered-ansi`. The task output decoder extracts diagnostics,
feeds Cargo's supplied human rendering to `TerminalScreen` and captured output, suppresses
machine-only artifact records, and passes non-JSON test/program output through. A small fallback
matcher recognizes Rust panic locations. Thus the human and agent query the same readable captured
stream.

### 4. Problems are protocol-neutral navigable state

Task output produces bounded `Problem` records containing workspace path, one-based line and
column, severity, and message. `problems.show`, `problems.next`, and `problems.previous` use the
normal filesystem-open path and move the editor cursor without editing the buffer. Paths outside
the workspace remain visible but non-navigable.

The action registry gains `task.cancel` and the three problem actions alongside the existing
`task.run`. Defaults are F5/Shift+F5 for run/cancel and F8/Shift+F8 for next/previous problem.

Normal Terminal focus keeps these keys shell-owned per ADR-0008, with one state-scoped exception:
`task.cancel` resolves while the *focused terminal is itself running a task*. That process is one
the IDE started and owns, and it is the thing the user is looking at; requiring them to leave the
pane to stop it would be a worse answer than letting the chord through. Everywhere else — and in
that same terminal once the task ends — the chord goes to the shell. ADR-0008 §3 is amended to name
this class of narrow, state-scoped reservation alongside copy mode.

`latest_problems` reads from the newest run that actually produced problems, not simply the newest
run: an agent starting a task of its own must not silently empty the list a human is working
through. Results persist until another run replaces them with results of its own.

### 5. Stable ACP invokes catalog tasks through exact terminal requests

Each agent turn's workspace context includes the same bounded catalog shown to the human: action
id, task id, label, exact structured command, and cwd. It describes the stable invocation as an
ACP `terminal/create` matching that catalog entry.

The model classifies an agent terminal request as a task only when program, every argv element,
and cwd exactly match the current catalog. This comparison does not authorize execution. The
Phase-04 one-shot or safe exact persistent grant must already permit the request, or the normal
permission prompt is shown. Near-matches and arbitrary commands remain ordinary agent terminals.

For Phase 05, “agent can invoke `task.run`” therefore means that the agent can invoke the same
catalog entry, state transition, managed terminal, output decoder, cancellation, and problems as
the human action using ACP's stable terminal mechanism. It does not claim that stable ACP carries
a custom method literally named `task.run`.

A wire-level custom tool is deferred until ACP defines a stable, portable client-tool mechanism
or the product has enough evidence to justify the cost of an authenticated MCP sidecar.

## Consequences

**One execution path stays true.** Human tasks, agent tasks, interactive shells, and ordinary
commands continue to share PTY lifecycle, rendering, capture, and cleanup. Task-specific logic is
decoding and metadata, not process ownership.

**The action surface grows deliberately.** Search shortcuts and task/problem actions are reachable
from the keymap and palette. `Ctrl+P` becomes Quick Open, while workspace search and the command
palette use `F9` and `F10`. The originally proposed `Ctrl+Shift+F/P` bindings are not portable:
legacy terminal input discards Shift and sends the same control bytes as `Ctrl+F/P`.

Because `F9`–`F12` are media keys on a stock macOS keyboard, both also get an `Alt` alias
(`Alt+F`, `Alt+P`) on the existing `F4`/`Alt+I` pattern: the function key carries the portability
promise, the `Alt` chord carries the ergonomics, and no default shortcut depends on a function key
alone. Note that this changes documented Phase 01–04 muscle memory — `Ctrl+P` was the palette — and
brings the code in line with ARCHITECTURE §6.3, which already specified `Ctrl+P` as quick-open.

**Agent portability wins over a literal tool name.** Every ACP agent with stable terminal support
can run catalog tasks. The limitation is documented rather than hidden, and exact matching leaves
ordinary command behavior intact.

**Ripgrep is an optimization, not an installation requirement.** If `rg` is missing, Quick Open
and workspace text search fall back to native gitignore-aware work on the search worker. Tests use
a helper process and do not require a developer-installed ripgrep.

**No new heavy dependency.** Ripgrep is a process integration chosen by ARCHITECTURE.md; Cargo
message parsing uses existing JSON support. The search and task crates opt into only the existing
internal and lightweight workspace dependencies they need.

**Fakes are mandatory.** `test-support` gains scripted search and fake task services. Search,
task selection, PTY execution, cancellation, problem matching, navigation, agent classification,
and rendering are deterministic without `rg`, Cargo, a real PTY, or an ACP agent.

**Later adapters stay narrow.** Maven, Gradle, Rake, Node, Python, Go, and other adapters add
detection, catalog entries, and output interpretation; they do not create new task runners or UI
subsystems.

**Deferred deliberately.** Regex and replace-in-files, custom task configuration, Cargo target
discovery, non-Cargo adapters, persistent Problems pane, LSP diagnostic merging, and custom MCP
tool transport are not Phase 05.
