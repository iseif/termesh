# Terminal-Native, Agent-First IDE — Masterplan

> **Name:** `termesh` (chosen at 0.1.0 — see §21 Branding, ADR-0004)
> **Status:** Pre-implementation design. This is a living document; major changes go through the RFC process (§20).
> **One-line positioning:** *An open-source, terminal-native IDE where the AI coding agent is a first-class occupant of the workspace — not a bolted-on panel — built as an ACP client so any compatible agent plugs in.*

---

## 0. How to read this document

This masterplan merges two inputs:

1. A strong execution scaffold (architecture style, service boundaries, transaction model, testing strategy, phasing rigor, OSS governance, performance targets).
2. A clear product **wedge**: the agent is structural. It shares the editor's live project context — open buffers, LSP diagnostics, git diff, terminal output — and proposes edits that render as reviewable diffs in the editor pane.

Where a generic "integrated terminal IDE" plan would defer AI to a post-1.0 plugin, this plan puts the agent loop **in the core and early in the schedule**, because that is the only part of the concept that is genuinely underserved in 2026. Everything else (file tree, editor, terminals, git) exists in mature form already; our reason to exist is the agent integration, so we prove it first.

---

## 1. Executive verdict

Build it, but commit hard to the wedge. A terminal IDE that merely combines a file tree, an editor, terminals, and a git panel is walking into a crowded room — the Zellij + Yazi + Helix + lazygit orchestration niche is already occupied by mature "glorified config" projects (yazelix, zide, zelix). Re-implementing that space natively, without a differentiator, produces a year of undifferentiated work whose honest review is "why not just use yazelix."

The differentiator is the agent. In the last year the industry standardized the editor↔agent interface via the **Agent Client Protocol (ACP)** — an open JSON-RPC 2.0 standard created by Zed (August 2025), co-maintained with JetBrains, with a shared agent registry and dozens of compatible agents (Claude Code, Codex, Gemini CLI, Copilot CLI, Goose, OpenCode). ACP has native support in Zed and JetBrains and community plugins in Neovim/Emacs — but **no purpose-built, terminal-native ACP-first IDE exists.** That is the gap.

So the promise is narrow and current:

> Not "JetBrains in a terminal." Build **the most capable agent-native developer workspace that runs anywhere a terminal runs**, on an open agent standard.

---

## 2. The product gap and differentiation

### 2.1 What already exists (and what we will not out-build)

The terminal already has excellent *focused* tools, and we integrate or learn from them rather than beat them at depth:

- **Helix** — modern modal editor; its CodeMirror-6-inspired core (rope + OT-style transactions) is our reference architecture.
- **Zellij** — terminal workspace/multiplexer; the orchestration model we replace with a native, state-sharing one.
- **Yazi** — fast file manager.
- **lazygit** — focused git TUI; a fine fallback for advanced git we don't build.
- **alacritty_terminal** — reusable VT parser/grid we embed rather than hand-roll.

### 2.2 Primary differentiation

1. **Agent as a structural peer.** The agent operates on the same workspace model as the human, through the same action registry, and its edits flow through the same transaction system. It is not a chat box stapled to the side.
2. **ACP-first, agent-agnostic.** We implement the ACP *client*. Any ACP agent works out of the box; we never marry one model vendor. Our competitive surface is the *quality of the editor-side integration*, not the agent.
3. **Terminal-native and remote-first.** Works over SSH, in containers, on minimal servers, with no desktop session; restores the workspace on reconnect. Zed's agent integration is a GPU GUI; ours runs where the code actually lives.
4. **Discoverable without modal-editor knowledge.** Familiar default keymap, command palette, visible shortcut hints, optional Vim/Emacs profiles later.
5. **Open platform.** Stable action registry and documented service traits from day one; sandboxed plugins much later.

### 2.3 Honest scope of the ACP bet

ACP is a subprocess protocol: the agent runs as a child process and communicates over stdio; it does **not** hold a pointer into our rope. That is the correct boundary, not a limitation. "Shared state" is achieved by making the **editor the single source of truth** and exposing rich, live context to the agent, while the agent proposes **changesets against a base revision** that we review, rebase, or reject. This maps cleanly onto the transaction spine (§8) and the ACP edit-proposal/permission flow (§9).

If a future thesis genuinely requires deeper in-process coupling than ACP expresses, that is the one decision that would justify a bespoke agent layer — but we do not start there, and we design so ACP is swappable.

---

## 3. The core thesis: agent as a structural peer

Two actors mutate one workspace: the **human** (via keybindings/mouse) and the **agent** (via ACP tool calls). The design keystone is that both drive the same internal command surface:

```
        Human input ──▶ ┐
                        ├──▶  Action Registry  ──▶  Command / Transaction  ──▶  Single-owner State
   Agent (ACP tools) ──▶ ┘                                                             │
                                                                                       ▼
                                                                          View Model ──▶ Renderer
```

- The **action registry** (`file.open`, `editor.apply_transaction`, `terminal.run`, `git.stage`, `search.workspace`, `lsp.goto_definition`, …) is simultaneously the human's command palette, the keymap target, and the **agent's tool schema exposed over ACP**. One registry, three front-ends.
- Because the agent's capabilities *are* the IDE's own commands, agent integration is nearly free once the registry and service traits are stable — and every capability we add to the human is automatically available (permission-gated) to the agent.

This is the single most important architectural idea in the document. Build the registry and service boundaries well and the "AI-native" property falls out; build them poorly and the agent will be a special-cased hack forever.

---

## 4. Target users

**Primary**

- Developers working over SSH / in containers / on remote or cloud workspaces who want agent-assisted editing where their code runs.
- Backend and infrastructure engineers living in the terminal.
- Developers who want an open, agent-agnostic alternative to GUI agent IDEs.
- Terminal users who want more discoverability than raw Neovim.

**Secondary**

- Developers on low-resource machines.
- Students and contributors in Codespaces-style environments.
- Anyone who wants one binary instead of assembling a multiplexer + file manager + editor + git TUI + agent glue.

---

## 5. Product principles

1. **Agent-native, not agent-bolted.** Every core surface (editor, terminal, git, search, diagnostics) is designed to be both human-operable and agent-operable through the action registry.
2. **Discoverable before configurable.** Useful on first launch; shortcut hints, menus, and the palette reduce memorization.
3. **Keyboard-first, not keyboard-only.** Mouse improves navigation/selection/resize where the terminal supports it.
4. **One shared project context.** Editor, terminal, git, tasks, diagnostics, search, and the agent all operate on the same workspace model.
5. **External tools remain first-class.** Git, ripgrep, language servers, formatters, debuggers, and ACP agents are integrated, not reimplemented.
6. **Every mutation is a reviewable transaction.** Human keystroke, paste, formatter, LSP edit, and agent proposal share one edit path — which is what makes agent diff-review safe.
7. **Fast feedback over decoration.** No blocking work on the render thread, ever.
8. **No silent telemetry.** OSS build ships telemetry-disabled; any future collection is explicit and opt-in.
9. **Explicit agent permissions.** File writes, command execution, and network access requested by an agent are permission-gated and visible.

---

## 6. Proposed user experience

### 6.1 Default layout — the agent is a first-class tool window

```text
┌ Project ───────────────┬ editor tabs ─────────────────────────────┬ Agent ──────────────┐
│ ▾ src                  │ main.rs •│ app.rs │ README.md            │ ▸ claude-code        │
│   ▾ services           ├──────────────────────────────────────────┤ session: refactor    │
│       git.rs           │  12   fn apply(&mut self, tx: Tx) {      │                      │
│       lsp.rs           │  13 ~     self.rope.apply(&tx);   ◀ diff  │ ▎ Proposed 3 edits   │
│   main.rs              │  14 +     self.version += 1;      ◀ diff  │ ▎ in editor.rs       │
│ Cargo.toml             │  15   }                                  │ ▎ [a]ccept [r]eject  │
│ README.md              │                                          │ ▎ [d]iff  [e]xplain  │
├────────────────────────┼──────────────────────────────────────────┤ …streaming…          │
│ Outline│Git│Tasks│Probs │ Terminal 1 │ Terminal 2 │ Output          │ tool: run cargo test │
│  ~ 2 changed           │ $ cargo test  (running…)                 │  ⏵ awaiting approval │
└────────────────────────┴──────────────────────────────────────────┴──────────────────────┘
 Ctrl+P Files  Ctrl+⇧P Actions  F6 Term  F7 Agent  branch: main ↑1  0 errors  ● claude-code
```

The **Agent pane** is a structural tool window (peer of Terminal/Problems/Git), not a modal popup. Agent-proposed edits render **inline in the editor** as accept/reject diff hunks; agent tool calls (run command, write file) surface as **permission prompts** with the exact command/args shown.

### 6.2 Interaction model — universal action registry

Menus, keybindings, command palette, context menus, plugins, **and the agent** all invoke named actions:

```
file.open · file.save · workspace.search · pane.split_right · terminal.new · terminal.run
git.stage · git.commit · task.run · editor.goto_definition · editor.apply_transaction
agent.session.new · agent.prompt · agent.proposal.accept · agent.proposal.reject
```

No behavior is hard-coded into widgets. This is the foundation for customization, plugins, and other integrations alike. Stable ACP does not currently provide portable client-owned custom-tool registration (ADR-0009).

### 6.3 Core navigation (defaults, all remappable)

- `Ctrl+P` quick-open file · `F9`/`Alt+F` workspace search · `F10`/`Alt+P` command palette
- `F6` toggle terminal · `F1`/`F2`/`F7` focus project/editor/agent · `F4` ask the agent
  - *Not `Ctrl+I`.* Without the kitty keyboard protocol a terminal sends `Ctrl+I` as byte
    `0x09` — the same byte as `Tab` — so binding it silently cycles focus instead. The same
    trap applies to `Ctrl+M`/`Enter`, `Ctrl+J`, `Ctrl+H`/`Backspace`, `Ctrl+[`/`Esc`, and
    every `Ctrl+Shift+letter` chord: legacy terminals discard Shift and send the same byte as
    `Ctrl+letter`. `F9`/`F10` carry workspace search and the palette for this reason;
    `KeyChord::is_terminal_ambiguous` names the set and the default keymap is tested
    against it. `Alt` is no safer as a *guarantee*: macOS Terminal.app sends Option as a
    compose key unless configured otherwise, so `Alt+I` is offered as an alias and a
    function key carries the promise.
- `Ctrl+Tab` switch tabs · `Alt+1..9` focus tool windows
- `F5` run task · `Shift+F5` cancel the newest running task
- `F8`/`Shift+F8` next/previous problem · `F12` go to definition · `Esc` dismiss overlay
  - A focused terminal is shell-first and swallows these, with two state-scoped
    exceptions: terminal **copy mode**, and `Shift+F5` while the focused terminal is
    running a task — cancelling the thing you are watching should not require leaving the
    pane first. Pane-focus chords are always reserved (ADR-0008 §3, ADR-0009 §4).
  - `F9`–`F12` are media keys on macOS unless "Use F1, F2, etc. as standard function keys"
    is enabled. The function key stays the portable *guarantee*; `Alt+F`/`Alt+P` are the
    ergonomic aliases, the same split already used for `F4`/`Alt+I`. A default shortcut
    is never reachable through a function key alone.
- Agent review: `a` accept hunk · `A` accept all · `r` reject · `d` show diff · `e` ask agent to explain

Default keymap targets mainstream-IDE muscle memory; Vim/Emacs profiles arrive later as keymap files, not core changes.

---

## 7. Architecture

### 7.1 Architectural style — single-owner state, message bus

```text
Terminal input · File events · PTY output · LSP messages · Task output · ACP session/update
                                         │
                                         ▼
                                   App Message Bus
                                         │
                                         ▼
                            State Reducer / Command Handler
                             │                         │
                             │                         └── Async effects & services (off-loop)
                             ▼
                       View Model (snapshot)
                             │
                             ▼
                          Renderer (Ratatui)
```

Rules:

- **One task owns all workspace state.** Everyone else *messages* it (Rust: owning task + `mpsc` in, `broadcast`/watch out). The Elm-style Model→Update→View loop is the same shape and enforces this naturally.
- Widgets render state and emit actions; they **never** call filesystem, git, PTY, LSP, or ACP services directly.
- All long-running work runs off the render loop and returns as a **typed application message**.
- Cancellation tokens guard searches, indexing, LSP requests, and in-flight agent turns.
- Focus is centrally managed; overlays are a stack.

The agent thread is just another async service: it receives commands (prompt, accept, cancel) and emits typed messages (`AgentStreamedText`, `AgentProposedEdits`, `AgentToolPermissionRequested`). It never mutates state directly — it proposes.

### 7.2 Crate layout

```text
crates/
  app/           Bootstrap, main loop, wiring
  core/          IDs, messages, actions, commands, shared types
  ui/            Layout, widgets, themes, focus, overlays
  workspace/     Project roots, sessions, settings, project detection
  filesystem/    Tree model, watching, mutations, ignore rules
  editor/        Buffers, cursors, selections, transactions, undo, rendering
  syntax/        Tree-sitter grammars and highlighting
  terminal/      PTY sessions + terminal state (parser/grid split)
  search/        File finder + content search
  git/           Repository model + git backend
  tasks/         Task discovery, execution, output, cancellation
  lsp/           Language-server processes + JSON-RPC client
  agent/         ACP client, session lifecycle, context assembly, proposals   ◀ the wedge
  config/        Global/workspace config + keymaps
  platform/      Clipboard, shell, paths, OS integration
  test-support/  Fixtures, fake services, recorded streams, render snapshots
```

`agent/` depends on `core/` (actions), `editor/` (transactions/diagnostics view), `filesystem/`, `terminal/`, `git/`, and `lsp/` **only through their service traits** — never their internals. This keeps the agent a pure consumer of the same surface the human uses.

### 7.3 State model — typed IDs, never paths as identity

Stable typed identifiers: `WorkspaceId`, `PaneId`, `BufferId`, `DocumentId`, `TerminalId`, `TaskRunId`, `LspServerId`, and for the agent layer: `AgentId`, `SessionId`, `TurnId`, `ProposalId`, `PermissionRequestId`.

Files can be renamed, symlinked, or unsaved — never key UI identity on a filesystem path.

### 7.4 Service boundaries — traits defined before implementations

```
FileSystemService · SearchService · GitService · PtyService · TaskService
LanguageService   · ClipboardService · SessionStore · AgentService
```

`AgentService` abstracts the ACP transport so the agent layer is testable against a scripted agent and so a non-ACP backend could be substituted later. Every trait enables deterministic tests (fake filesystem, fake PTY, scripted LSP, scripted agent).

---

## 8. The shared-state transaction spine (the heart)

Both actors change buffers, so **no direct mutation** — every change is a transaction stamped with the base version. This is Helix/CodeMirror-6's model, chosen precisely because it also solves human/agent concurrency.

```
EditTransaction
  base_version:    u64            // buffer revision this was authored against
  changes:         ChangeSet      // ordered, position-composable text ops
  selection_map:   SelectionMap   // how cursors/selections transform
  undo_group:      UndoGroupId
  source:          Keyboard | Paste | Formatter | Lsp | Agent(ProposalId) | Replace
```

Properties this buys us:

- **Single undo/redo path** for all sources.
- **LSP synchronization** derives from the same change stream (`didChange` versions).
- **Incremental syntax updates** feed tree-sitter from the change stream.
- **Agent diff-review, safely.** An agent proposal is a `ChangeSet` with `base_version = N`. On accept:
  - if the buffer is still at `N`, apply directly;
  - if the human typed since (buffer at `N+k`), **rebase** the proposal's positions forward through the intervening transactions (position-mapping is exactly what ChangeSets provide) and apply, or reject and re-ask if it no longer composes cleanly.

This is lightweight OT — not full CRDT collaboration — and it is enough because there is exactly one authoritative buffer and the agent is asynchronous. Getting this spine right in Phase 03 is what makes the entire agent feature tractable; everything downstream depends on it.

**Reuse vs. build:** use `ropey` for the rope; build the transaction/proposal layer ourselves to fit multi-actor edits, borrowing Helix's `Transaction`/`ChangeSet`/`Selection` design directly. (Helix's crates are still `0.0.0` placeholders on crates.io, so depend via git or vendor the relevant modules; the value is the design, which is documented and battle-tested.)

---

## 9. The agent layer: ACP client architecture

This is the section that justifies the project. We implement the **client** side of ACP.

### 9.1 Protocol shape

- **Transport:** JSON-RPC 2.0 over stdio to a locally spawned agent subprocess (also supports remote per the spec). We are the **client**; the agent is the **server**.
- **Lifecycle:** initialize/capability negotiation → `session/new` → `session/prompt` (user turn) → a stream of `session/update` notifications carrying streamed assistant text, tool-call requests, and edit proposals → turn completion. Cancellation ends the current turn cleanly.
- **Registry:** support agents discovered via the ACP Registry plus user-configured local agent commands.
- Follow the canonical spec at `agentclientprotocol.com`; do not freeze our own fork of the wire format.

### 9.2 Context assembly — what makes it "shared state"

On `session/new` and as context evolves, the editor forwards a curated, live view of the workspace:

- workspace root, project type, git branch/status/diff;
- open buffers with paths, dirty state, and current selection/cursor;
- current LSP diagnostics and, on request, symbols/definitions;
- recent terminal output (bounded) when the agent needs command results.
- the bounded current task catalog, including each exact program, argv, and cwd.

The agent never guesses at `@`-mention gymnastics — the editor *is* its context provider, because the editor already owns all of it as single source of truth. This is the concrete meaning of "the agent shares your buffer/LSP/diagnostics/git state."

The mechanism is ACP's own: `fs/read_text_file` is implemented by the **client**, so the agent asks *us* for file contents and we answer **from the live buffer, including unsaved changes**, falling back to the filesystem only for files that are not open. The agent and the human therefore read the same bytes because there is only one copy — the property a bolt-on integration cannot have. Context is *pulled* on demand rather than pushed wholesale into every turn, so it cannot go stale; only the small workspace snapshot (root, project kind, visible tree, selection) is attached per turn. See ADR-0007 §3, §4.

### 9.3 Edit proposals → reviewable diffs

Agent edits arrive as **whole-file before/after text** (`ToolCallContent::Diff { path, old_text, new_text }`) — *not* as range edits, as an earlier draft of this section assumed. We derive a `ChangeSet` by diffing `old_text → new_text`, split it into hunks at line granularity, and convert it into an `EditTransaction` with `source = Agent(ProposalId)`.

`base_version` is not "captured at assembly time" but **recorded when we serve the file**: because we implement ACP's client-side `fs/read_text_file` (§9.2), we know exactly which buffer version the agent read, and that version anchors the proposal. When `old_text` matches what we served, the proposal rebases through the ordinary transaction spine; when it does not (the agent read the file by some other means), we anchor by surrounding context or mark the hunk conflicted rather than guessing.

Hunks render **inline in the editor** as accept/reject (per-hunk and per-proposal granularity). Accept routes through the transaction spine (§8) with rebasing; reject drops the proposal. Nothing the agent proposes touches disk until the human (or a configured auto-accept policy) accepts. Details in ADR-0006 and ADR-0007.

### 9.4 Tool calls and permissions

When the agent requests a tool (run a command, write a file, read outside the workspace, network), we surface a **permission request** showing the exact operation (command + argv, target path). Approved command execution runs in a **managed PTY** (§11) with output streamed back to the agent as a tool result. Argument arrays only — never interpolate agent output into a shell string.

The protocol's response vocabulary is `AllowOnce | AllowAlways | RejectOnce | RejectAlways` (four options, not the three an earlier draft of this section implied — `RejectAlways` has no equivalent in a "prompt / allow-list / session-grant" model and must round-trip). Default policy is prompt-always; `AllowAlways` grants are recorded per workspace and never silently broadened. Because a permission response is binary, a **partial** accept — the human takes some hunks and not others — is answered `RejectOnce` plus a follow-up message naming what was applied, so the agent never proceeds believing the file matches its proposal. See ADR-0007 §6, §8.

Stable ACP has no portable client-owned custom-tool method for `task.run`. Phase 05 therefore
publishes the same task catalog in turn context and recognizes a standard ACP `terminal/create`
as a task only when its program, complete argv, cwd, and empty environment exactly match a current
catalog entry. Recognition happens **after** the ordinary permission policy permits execution; it
never creates a grant. Exact matches share the human task lifecycle, decoder, cancellation, and
Problems state, while near-matches remain ordinary agent terminals. See ADR-0009.

### 9.5 Parallel agents (later)

The architecture (one owner, message bus, per-session state) supports multiple concurrent agent sessions — e.g., one refactoring while another writes tests — each in its own `SessionId` with isolated proposals. This is a headline capability of GUI agent IDEs and a natural terminal-native differentiator, but it is deferred until a single session is rock-solid (§16, Phase 11).

### 9.6 Why ACP over a bespoke loop

- **Instant ecosystem:** Claude Code, Codex, Gemini, OpenCode, Goose all work day one.
- **No vendor lock:** users bring their own agent and model.
- **Right boundary:** subprocess + proposals-against-revisions is exactly the safe concurrency model we want.
- **Standards alignment:** we ride an interface the ecosystem already converged on, and our effort concentrates on the *editor-side UX* where we can actually be best-in-class.

---

## 10. Editor design

Highest-risk component; intentionally limited at first.

**Buffer model:** rope-backed text; path-or-untitled identity; encoding + line-ending metadata; version counter; dirty flag; cursor set; selection set; undo/transaction history; syntax tree; diagnostic overlays; **agent-proposal overlays**.

**V1 editor limits (ship discipline):** single cursor; no macros; no rectangular selection; no structural editing; no Vim command language. Multiple cursors come *after* the transaction model is proven — and they compose with it for free once it is.

The editor must render three overlay classes cleanly from day one: syntax highlight, LSP diagnostics, and agent-proposed diff hunks. Designing the overlay/decoration system with the agent hunks in mind (not retrofitted) is a Phase-03 requirement, not a Phase-07 afterthought.

---

## 11. Terminal design

An embedded terminal is a small terminal emulator, and it is the most commonly underestimated component. Split it hard:

```
PTY process (portable-pty) ──bytes──▶ VT parser + grid (alacritty_terminal) ──▶ screen model ──▶ renderer
```

Each session owns: a PTY process; an ANSI/VT parser + screen grid; cursor/mode state; bounded scrollback; input translation; resize propagation; process-exit state.

- **Reuse `alacritty_terminal`** (the grid/parser engine Zed's terminal uses) rather than writing a VT state machine.
- Keep the PTY lifecycle separate from the screen model so it can be tested against **recorded byte streams**.
- The managed terminal is also the execution surface for agent-approved commands (§9.4): same code path, extra permission gate and result capture.
- Terminal focus is shell-first: input is encoded for the PTY before global bindings; `F6` is the explicit focus/escape chord, the `Tab` ring skips the pane so it cannot strand the user, and copy mode is a separate state.
- ACP integration uses the standard `terminal/create`, `terminal/output`, `terminal/wait_for_exit`, `terminal/kill`, and `terminal/release` methods. Wire IDs stay in the agent translator; the model sees typed terminal IDs and bounded captured output.
- A terminal tab may restart with the same `TerminalId`; every spawned process has a monotonically increasing generation, so callbacks from a detached reader can never mutate the replacement process's screen or capture.

**Safety:** never interpolate input into shell strings; launch with argv arrays; always display working directory and command; preserve normal interactive shell behavior.

---

## 12. Language server architecture

The IDE is an LSP client, and the agent consumes LSP results as context.

Responsibilities: start/stop servers; negotiate capabilities; synchronize document versions (from the transaction stream); route + cancel requests; convert UTF-8 editor positions to the server's expected encoding; restart crashed servers with backoff; expose server logs.

Ship **recipes, not bundled servers** (auto-detect installed servers; actionable setup message when missing):

- **Implemented:** Rust → rust-analyzer · TS/JS → typescript-language-server · Python → pyright · Java → Eclipse JDT LS through the `jdtls` launcher.
- **Intended:** Go → gopls and Dart/Flutter → Dart analysis server (post-beta).

Diagnostics and symbols flow into both the editor overlays **and** the agent context assembly (§9.2) — the same data, two consumers.

---

## 13. Configuration

```
~/.config/<app>/config.toml       # theme, shell, tabs, wrap, autosave, keymap profile, exclusions
~/.config/<app>/keymap.toml
~/.config/<app>/themes/
~/.config/<app>/agents.toml        # ACP agent commands, permission policies, default agent
<project>/.<app>/workspace.toml    # tasks, LSP/formatter commands, per-project agent + permissions, env
```

Config errors surface **inside the app** with file, line, explanation, and the fallback taken. Per-workspace agent permission policy lives in `workspace.toml` and is never silently broadened.

**0.1.0 boundary:** `config.toml` and `keymap.toml` ship as versioned, layered configuration.
`themes/` is deferred to Phase 11 because the compiled theme-token layer has no Phase 10 feature
that needs a user-theme loader. The `soft_wrap` key is parsed and preserved but not applied until
the editor viewport and decoration model can become wrap-aware. Both deferrals are explicit in
ADR-0014 §1 and [docs/support.md](docs/support.md), rather than represented by placeholder loaders.

---

## 14. MVP scope (reprioritized and trimmed)

The MVP proves the **agent loop over a real editor**, not breadth. Deliberately narrower than a generic terminal-IDE MVP.

- **Workspace:** open a directory; project-root + type detection; `.gitignore` respect; session restore (open files, selection, pane sizes, terminal cwd, active tool window, **active agent session**).
- **File explorer:** lazy tree; create/rename/move/delete; fuzzy filter; git-status decorations; reveal current file.
- **Editor core:** tabs; UTF-8 editing; **transaction-based** undo/redo; single-cursor selection; clipboard; find/replace; line numbers; dirty indicators; syntax highlighting (small language set); **agent diff-hunk overlays**.
- **Terminals:** multiple PTY tabs; shell auto-detect; scrollback; resize; ANSI; managed-command execution path for the agent.
- **Search:** fuzzy file finder; ripgrep-fast content search with a native fallback and preview.
- **Command palette:** searchable actions with shortcut labels and context awareness, backed by the shared action registry.
- **Git essentials (trimmed):** status model; changed-file list; **diff viewer**; commit; fetch/pull/push; branch checkout. *Hunk-level staging, rebases, and reflog are deferred or delegate to lazygit.*
- **Language intelligence (polyglot):** diagnostics; go-to-definition; hover; completion; references; document and workspace symbols; rename; code actions; and formatting for **Rust via rust-analyzer, TypeScript/JavaScript via typescript-language-server, Python via pyright, and Java via Eclipse JDT LS**, with diagnostics and symbols included in agent context.
- **Agent (the point):** ACP client; one agent configured (e.g., Claude Code or OpenCode); `session/new` + prompt + streaming; **edit proposals → inline accept/reject**; permission-gated command execution in the managed terminal; context assembly from open buffers + diagnostics + git diff.

**Cut from MVP vs. a generic plan:** full hunk staging → status+diff+commit; plugin system → none; parallel agents → none; debugger → none.

Two of these were re-sequenced rather than cut. Task adapters and language recipes were scoped to one each (Rust/Cargo) so the agent loop could be proved first; §16 Phases 08–09 then add TS/JS, Python, and Java *before* `0.1.0`, because the seam that carries them was cheaper to validate while fresh than after a beta (ADR-0012). The principle stands unchanged — breadth is not the differentiator and is never what proves the wedge — but the MVP that ships as `0.1.0` is no longer single-language.

---

## 15. Explicit non-goals for V1

The AI agent is **not** here — it is the core (§9). Non-goals are:

- perfect Vim/Emacs compatibility;
- a custom compiler or language-analysis engine;
- GUI parity with JetBrains;
- collaborative/multiplayer editing;
- remote filesystem sync;
- full Debug Adapter Protocol UI;
- third-party plugin marketplace;
- notebooks, database browser, visual designers;
- **parallel/multi-agent** orchestration (post-MVP);
- **bespoke (non-ACP) agent protocol** (only if a proven need emerges).

---

## 16. Development phases

Reordered so the wedge is proved early. The rule: **the agent diff-review loop is demoable by the end of Phase 03**, on a thin editor, before tasks/git/full-LSP breadth exists.

**Phase 00 — Foundation.** Repo, license (§18), ADRs, README/CONTRIBUTING/CODE_OF_CONDUCT/SECURITY, Rust workspace, cross-platform CI (Linux/macOS/Windows), fmt/lint/test/audit, templates, terminology glossary.
*Exit:* clone → build → test → run a blank shell.

**Phase 01 — TUI shell.** Terminal init/restore, event loop, resize, focus manager, pane layout engine, overlay stack, status bar, **action registry**, command palette, configurable keymap, theme tokens.
*Exit:* move focus, resize panes, invoke actions, open overlays reliably.

**Phase 02 — Workspace + file explorer.** CLI open, root detection, lazy tree, `.gitignore`, file ops, recent workspaces, session persistence, file watching.
*Exit:* useful as a project browser/viewer.

**Phase 03 — Editor core + the transaction spine + first agent loop.** Rope buffers; load/save; cursor/selection; **EditTransaction/ChangeSet**; undo/redo; tabs; find/replace; syntax highlighting (small set); overlay/decoration system incl. **diff hunks**. Then: minimal **ACP client** — spawn one agent, `session/new`, prompt, stream text, receive edits → render as accept/reject hunks (rebased through §8).
*Exit:* **the demo** — type in a file, ask the agent to change it, review its edits as inline diffs, accept, undo. Works with **no LSP and no tasks yet.**

**Phase 04 — Integrated terminals + agent command execution.** portable-pty service; alacritty_terminal grid; terminal tabs; input/scrollback/copy; process status/restart; **managed-command execution with permission gate** feeding results back to the agent.
*Exit:* interactive shells/builds/tests work cross-platform; agent can run an approved command and read its output.

**Phase 05 — Search + tasks + project awareness (trimmed).** Quick-open; ripgrep-fast content search with native fallback + preview; project-type detection; **Cargo task adapter first**; task output + cancellation; basic problem matching. Agent can invoke `task.run` (permissioned).
*Exit:* open a project, search, run tests, jump to failures — human or agent.

**Phase 06 — Git integration (trimmed).** Status model; branch selector; **diff viewer**; commit; fetch/pull/push; conflict indicators; git actions in palette. Agent sees git diff as context and can propose commits (human approves).
*Exit:* edit→review→commit→push works.

**Phase 07 — Language intelligence + richer context.** LSP process manager; doc sync from the transaction stream; diagnostics panel + editor decorations; hover/completion/definition/references/symbols/rename/code-actions/formatting for the flagship language; **diagnostics + symbols piped into agent context.**
*Exit:* Rust fully works through documented recipes; the agent reasons over live diagnostics.

**Phase 08 — Polyglot workspaces (TypeScript/JavaScript + Python).** Project detection reports every kind at the root; language sessions start lazily on the first document they claim; TS/JS and Python recipes; task discovery reads project configuration (npm scripts from `package.json`, conventional Python tasks, `[tasks.*]` workspace overrides); one text problem matcher for non-Cargo output.
*Exit:* a repository holding two toolchains runs both servers, and `F5` lists the tasks each project actually declares.

**Phase 09 — Java.** Eclipse JDT LS through a wrapper-owned `jdtls` launcher; Maven and Gradle conventional tasks with project-wrapper preference; JDT import progress and build-file reload; navigable javac failures.
*Exit:* a Maven or Gradle project has working diagnostics, navigation, and tasks.

**Phase 10 — Public beta hardening.** Crash recovery; perf profiling; large-project tests; low-color/narrow-SSH terminal testing; keymap conflict review; onboarding + built-in help; config migration; signed binaries; Homebrew/Scoop/WinGet/Cargo/Linux packaging; docs site + demo recording.
*Exit:* `0.1.0` beta with clear support boundaries.

Phase 10 supplies the release workflow, package templates, local documentation, and exact owner
checklist. Certificate provisioning, signing verification with real credentials, registry/package
manager publication, the hosted docs site, demo recording, tag, and GitHub release are deliberately
owner-executed; they are irreversible or require credentials (ADR-0014).

**Phase 11 — Post-beta platform features.** Remaining LSP recipes (Go, Dart); **parallel/multi-agent sessions**; DAP debugger UI; plugin SDK (staged, §17); remote/SSH profiles; test explorer; outline/breadcrumbs; PR workflows.

> **Why 08–09 precede hardening.** The original order put every additional recipe after `0.1.0`. It was changed after Phase 07 shipped, for two reasons recorded in ADR-0012: the polyglot seam built in Phase 07 (sessions keyed by `LspServerId`, per-document routing) had never been exercised by a second server and was cheaper to validate while the design was fresh; and hardening is partly a function of how many servers run, so hardening a one-language product would have to be redone. Java is its own phase because Eclipse JDT LS plus two build systems is not comparable in cost to the other recipes.

---

## 17. The first vertical slice

Build vertically, not months of isolated infrastructure. The slice ends on the differentiator:

1. open a Rust project;
2. render project tree, editor, and terminal panes;
3. open and edit a file (through the transaction spine);
4. save it;
5. run `cargo test` in a managed PTY;
6. select a compiler error, jump to file+line;
7. show the git modification in the tree/status bar;
8. **prompt the configured ACP agent to fix the failing test;**
9. **review the agent's proposed edits as inline accept/reject diffs; accept; re-run tests;**
10. restore the same workspace (including agent session) after restart.

Steps 8–9 are the whole reason the project exists — they must be in the *first* slice, not a later milestone. This slice also exposes every hard boundary early: transactions, PTY, LSP position mapping, git, ACP session + proposal rebasing, and session persistence.

---

## 18. Testing strategy

**Unit:** text transactions; cursor/selection transforms; undo grouping; UTF-8/grapheme handling; layout math; keymap resolution; file-tree reconciliation; LSP position conversion; **proposal rebasing against concurrent edits.**

**Golden/snapshot:** widget rendering at multiple sizes; themes + low-color fallback; overlays/focus; diff rendering; diagnostics; **agent diff-hunk rendering.**

**Integration (fakes for every service trait):** fake filesystem; fake PTY with recorded ANSI; fake git; scripted LSP; **scripted ACP agent** replaying `session/update` streams incl. edit proposals and tool-permission requests.

**End-to-end:** drive the app in a controlled pseudo-terminal, replay keyboard/mouse, assert final screen + filesystem + git state — including a full "prompt → propose → accept → tests pass" agent run.

**Cross-platform matrix:** Ubuntu, macOS, and Windows CI; real PTY integration plus headless
snapshots at narrow widths and true-colour, 256/16-colour, and no-colour depths. Named terminal
emulators are support claims only after a recorded manual run (`docs/support.md`).

---

## 19. Performance targets (engineering goals, not promises)

- Warm start < 150 ms on a typical laptop.
- Input-to-render < one frame under normal editing.
- No blocking FS/git/search/LSP/task/**agent** work on the UI thread — agent streaming must never stall typing.
- Idle CPU near zero; bounded terminal scrollback and agent-context caches.
- Lazy loading + exclusions keep repos with hundreds of thousands of files navigable.

Instrument event-loop delay, frame duration, task/agent-turn duration, queue depth, and per-subsystem memory. Local and developer-controlled only.

---

## 20. Plugin strategy (deferred; do not build early)

Stabilize the action registry and service traits first — the agent is the proof that these boundaries are right, so **the agent hardens the very APIs plugins will later use.**

- **Stage 1 — config extensions:** custom tasks, file openers, themes, keymaps, LSP/formatter/**agent** definitions.
- **Stage 2 — process extensions:** JSON-RPC, limited actions, explicit permissions.
- **Stage 3 — sandboxed WASM:** register actions, tool-window views, safe event subscriptions, approved host APIs, declared permissions. No unrestricted FS/process access by default.

---

## 21. Open-source strategy

**License:** dual **MIT / Apache-2.0** (or Apache-2.0 alone if explicit patent terms preferred).

**Repo standards from first commit:** architecture overview (this doc), roadmap, contribution guide, code of conduct, security policy, release + support policy, ADR directory, RFC process for major changes (esp. anything touching the action registry, transaction spine, or ACP client).

**Contributor experience:** one-command dev setup; deterministic fixtures incl. the scripted ACP agent; in-TUI component gallery; good-first-issue/help-wanted labels; module ownership; recordings in UI PRs; perf regression checks on hot paths.

**Governance:** benevolent-maintainer to start, documented: maintainer onboarding, RFC acceptance, compatibility expectations, review requirements, security disclosure.

**Branding (before naming):** check GitHub/registries/domains/trademarks; short, easy-to-type command; avoid names tied to existing editors/shells/terminals; reserve org + package names + domain together. Consider signaling the agent-native angle in the name without boxing into one model vendor.

---

## 22. Major risks and mitigations

- **Editor becomes the whole project.** → Cap V1 editor features; transaction model from day one; lean on tree-sitter/LSP/formatters; no Vim emulation in MVP.
- **Terminal emulation complexity.** → Split PTY/parser/screen/renderer; reuse `alacritty_terminal`; test against recorded ANSI corpora.
- **ACP spec churn / capability gaps.** → Isolate the wire format behind `AgentService`; track the spec; keep a scripted-agent test harness so upgrades are safe; contribute upstream rather than fork.
- **Agent edits corrupt buffers / bad concurrency.** → Proposals are transactions against a base version; rebase-or-reject; nothing hits disk without accept; comprehensive rebasing tests.
- **Unclear differentiation.** → Lead every message with agent-native + ACP + terminal/remote; never market as "modal editing done better."
- **Platform drift.** → Cross-platform CI from Phase 00; platform service abstractions; capability detection + documented fallbacks.
- **Scope creep toward a JetBrains clone.** → Strict release themes; RFCs for new subsystems; roadmap split into core / platform / optional.
- **Vendor perception (tied to one agent).** → Agent-agnostic ACP client; ship recipes for several agents; default is user's choice.

---

## 23. Definition of the first compelling release

Compelling when a developer can:

1. install one binary and run it in a project;
2. navigate the tree; open/edit multiple files;
3. search the whole project;
4. open multiple integrated terminals;
5. run project tasks/tests and jump to failures;
6. inspect changes, commit, and push;
7. get diagnostics/completion/navigation from an installed language server;
8. **configure an ACP agent, prompt it with full project context, and review/accept/reject its edits as inline diffs;**
9. **let the agent run permission-gated commands in the terminal and act on the results;**
10. close and later restore the workspace, including the agent session.

> **0.1.0 degradation for item 10:** the workspace, buffers, active tab, pane geometry, terminal
> working directories, and read-only transcript history are restored, but the ACP agent session is
> not. This client implements no `session/load` path and therefore starts a fresh session rather
> than pretending transcript replay is protocol-level continuity. See ADR-0014 §4 and
> [docs/support.md](docs/support.md).

Items 8–9 are the ones no mature terminal tool delivers today. Debugging, plugins, remote orchestration, and multi-agent follow only after this loop is dependable.

---

## 24. Final recommendation

Proceed — with a focused promise:

> Not "JetBrains inside a terminal." Build **the most capable, most approachable agent-native developer workspace for the terminal**, on the open ACP standard.

Use **Rust + Ratatui**; establish the **action registry and service traits** first; make **every edit a transaction** so agent diff-review is safe; implement the **ACP client** in Phase 03 and prove the agent loop before adding breadth; keep the MVP narrow (one language, one task adapter, status+diff git); and delay plugins, debugging, collaboration, and multi-agent until the core agent-native loop is dependable.

---

## Appendix A — Recommended technology foundation

| Area | Choice | Notes |
|---|---|---|
| Language | Rust | Single binary; low-latency; native FS/process/PTY/parsing/protocol work in one runtime. |
| TUI rendering | Ratatui | Fast, lightweight immediate-mode TUI. |
| Terminal backend/input | Crossterm | Cross-platform input + backend. |
| Async runtime | Tokio | Services, PTYs, LSP, ACP transport. |
| Text buffer | Ropey | UTF-8 rope; re-exported by Helix's core too. |
| Transactions | In-house | Modeled on Helix/CodeMirror-6 `ChangeSet`/`Transaction`/`Selection`. |
| Syntax | Tree-sitter | Incremental parsing fed by the change stream. |
| PTY | portable-pty | Cross-platform PTY API. |
| Terminal state/parser | alacritty_terminal | Reusable VT parser + grid (the engine Zed's terminal uses). |
| File watching | notify | Tree refresh. |
| Search | ripgrep (process) first, native `ignore` fallback | git2/gitoxide optional later. |
| Git | Git CLI first | `git2` backend optional later. |
| Agent transport | ACP client (JSON-RPC 2.0 / stdio) | Behind `AgentService`; spec at agentclientprotocol.com. |
| Config | TOML | + serde. |
| Logging | tracing | Local, opt-in metrics. |
| Errors | thiserror + anyhow | anyhow at app boundaries. |
| Testing | Rust tests + snapshots + scripted services | Incl. scripted ACP agent + recorded ANSI. |

**Why not TypeScript/Go/Python first:** the hard parts are text-buffer correctness, PTY/terminal emulation, cross-platform behavior, large-file performance, LSP orchestration, and the transaction/proposal spine. Rust keeps those in one native runtime with the strongest terminal-tool ecosystem (ropey, tree-sitter, alacritty_terminal, portable-pty) and — decisively — the same ecosystem Helix's reference architecture lives in. OpenTUI (Zig core + TS bindings) is a fine future extension-SDK surface, not the core.

## Appendix B — References

- Agent Client Protocol — spec & introduction: https://agentclientprotocol.com/get-started/introduction
- Agent Client Protocol — Zed overview & editor client: https://zed.dev/acp
- ACP Registry announcement: https://zed.dev/blog/acp-registry
- Helix editor architecture (rope, transactions, events): https://github.com/helix-editor/helix/blob/master/docs/architecture.md
- Ropey (UTF-8 rope): https://crates.io/crates/ropey
- Ratatui: https://ratatui.rs
- alacritty_terminal / portable-pty / notify: crates.io
