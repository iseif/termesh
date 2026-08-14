# Glossary

**Action / action registry** — Named, invocable operations (`file.open`, `agent.prompt`, …). One registry backs the keymap and command palette and provides the stable seam for later integrations (ARCHITECTURE.md §3). Stable ACP does not currently expose portable client-owned custom tools (ADR-0009).

**ACP (Agent Client Protocol)** — Open JSON-RPC 2.0 standard for editor↔agent communication (agent runs as a subprocess over stdio; the editor is the client). We implement the client. <https://agentclientprotocol.com>

**Agent integration tier** — Tier 0: run an AI CLI in a terminal pane (free). Tier 1: native ACP client with shared context and diff-review (the wedge). ADR-0003.

**AppMessage** — The typed message every off-loop producer sends to the single state owner (input pump, filesystem worker, later PTY/LSP/ACP). The main loop blocks on this one channel rather than on any single source. ARCHITECTURE.md §7.1.

**Buffer** — In-memory text of an open document; rope-backed, versioned, with cursors, undo history, syntax tree, and overlays.

**Colour depth** — The palette capability Termesh can safely use: true colour, 256 colour, 16 colour, or no colour. It is detected from the environment unless `--color` overrides it; `NO_COLOR` forces the no-colour fallback. The chosen depth changes tokens, not application state (ADR-0014).

**ChangeSet** — A complete traversal of a document as `Retain`/`Delete`/`Insert` steps, in **char** offsets. Because it covers the whole document it composes with another changeset and can map any position forward — which is what makes rebasing a total function rather than a special case. ADR-0006 §1.

**Command grant** — A persisted permission for one exact program + argv + workspace-relative cwd combination. Grants are workspace-scoped and never include environment overrides; any mismatch prompts again. ADR-0008 §5.

**Assoc** — Which side of an insertion a mapped position lands on. Only matters when text is inserted at exactly the position being mapped; pending proposal anchors always use `After`, so agent text lands after text the human already typed there. ADR-0006 §2.

**Decoration** — A styled overlay on buffer text: syntax, diagnostic, or agent hunk. All three classes exist from the start, because a layer built for highlighting alone assumes spans always exist in the buffer — which a proposed *insertion* does not. ARCHITECTURE.md §10, ADR-0006.

**Draft** — A debounced crash-recovery mirror of one dirty buffer, stored under the app config directory through `FileSystemService`. It never overwrites the user's file automatically: recovery is offered, and acceptance applies through an undoable `EditTransaction` (ADR-0014 §5).

**Display column** — A screen cell, as opposed to a char offset. The two disagree on any line with a tab or a wide glyph, so the conversion lives in exactly one place (`ui::text`) and everything that draws into the text area uses it.

**EditTransaction** — The single path by which a buffer changes, stamped with the base version and a source (keyboard, paste, formatter, LSP, agent, replace). ARCHITECTURE.md §8.

**Edit proposal** — Edits an agent wants to make, surfaced as inline accept/reject diff hunks. Nothing hits disk until accepted. **Authoritative**: it keeps the agent's original before/after text and recomputes its hunks from that fixed point, so the buffer's decorations are a *projection* of it rather than a second copy that can disagree.

**Hunk** — One contiguous change the human accepts or rejects on its own, at line granularity. A pure insertion is zero-width; a pure deletion has no replacement text. Review is per-hunk, so one conflict never invalidates its siblings (ARCHITECTURE.md §9.3).

**Hunk state** — `Clean` (applies as-is), `Conflicted` (the human's edit collided with it — shown with the reason, never applied over), or `Satisfied` (the human already made this exact change). ADR-0006 §4.

**GitRepositorySnapshot** — One bounded, protocol-neutral reading of a repository: repository and workspace roots, branch/upstream divergence, per-path index/worktree state, and separately labelled staged/worktree context diffs. Produced off-loop by `GitService`, then owned by the model. ADR-0010 §2.

**GitState** — The model-owned Git cache and request-correlation state: latest good `GitRepositorySnapshot`, selected diff/branches, loading/stale/unavailable state, active request IDs, and one coalesced pending refresh. Rendering and agent context read it without performing Git I/O. ADR-0010 §2.

**Explicit index commit boundary** — `git.commit` commits exactly what is already in Git's index. It never stages implicitly, refuses an empty index or blank message, and leaves worktree-only edits out of the commit. Whole-file stage/unstage is explicit; hunk staging is deferred. ADR-0010 §3.

**Read set** — What we served the agent through `fs/read_text_file`, and the buffer version it came from. Because *we* answer the agent's file reads, we know exactly what it read, and its proposal anchors to a revision we hold. ADR-0007 §5.

**Undo group** — The unit undo works in. A run of typing is one group; so is an accepted proposal, however many hunks it touched — which is what makes "accept, undo" undo *the agent's change* rather than one hunk of it. ADR-0006 §6.

**Coalescing window** — The short interval (default 100 ms) over which raw file-watch events are gathered before the tree re-reads. Measured from the first event of a batch, so a steady trickle still flushes on schedule. ADR-0005 §5.

**Ignore rules** — The `.gitignore`/`.ignore`/`.git/info/exclude` chain, plus the hidden-files policy, that decides what the explorer shows — and therefore what the agent sees. ADR-0005 §4.

**JDT LS** — Eclipse's Java language server. Termesh starts the `jdtls` launcher script lazily when the first `.java` file opens; the wrapper, not Termesh, owns equinox-jar selection, platform configuration, and workspace data. Vendor import progress and project-reload notifications are translated at the LSP boundary. ADR-0013.

**Lazy tree** — The file explorer's loading strategy: one directory level is read per expansion, never recursively. Collapsed subtrees cost nothing, which is what makes a monorepo root openable. ADR-0005 §2.

**LanguageService** — The typed boundary around one long-lived, bidirectional language-server session. Unlike request/reply workers, it can emit unsolicited diagnostics and progress and must answer server-to-client requests. Wire framing and JSON remain inside `crates/lsp`. ADR-0011.

**LSP (Language Server Protocol)** — Standard for language intelligence: diagnostics, navigation, hover, completion, symbols, rename, code actions, and formatting. Termesh is an LSP client; diagnostics and symbols also feed bounded agent context.

**LspState** — Model-owned language state containing idle configured recipes plus live sessions keyed by `LspServerId`: session readiness and request correlation, extension-based document routing, strictly increasing wire document versions, live diagnostics, and synchronized buffer shadows. A recipe moves from configuration to a live session only when its first claimed document opens. Rendering and agent context read this state without protocol I/O. ADR-0011, ADR-0012.

**Wire document version** — The strictly increasing version Termesh sends in `didOpen`/`didChange`. It is deliberately separate from `Buffer::version`, which can move backward on undo or reset on reload. ADR-0011 §4.

**Managed terminal** — The app-owned PTY, parser/grid, bounded capture, and lifecycle service shared by human shells and permission-gated ACP commands. OS handles remain inside the terminal service. ADR-0008.

**NodeId** — The identity of a file-tree node. Identity is the id, never the path: paths move under rename and watch events, and keying on them would lose the user's selection and expanded subtrees on every refresh. ARCHITECTURE.md §7.3.

**PTY** — Pseudo-terminal: the OS mechanism behind an interactive shell. See `portable-pty`.

**Problem** — A bounded, navigable diagnostic decoded from task output: path, one-based line and column, severity, and message. Problems outside the workspace remain visible but cannot be opened; relative paths that traverse above the task cwd are rejected. ADR-0009.

**Rebase (proposal)** — Carrying an agent proposal onto the buffer as it stands now. The agent's base is not necessarily a revision we hold, so rather than replaying undo history we *synthesise* the intervening change by diffing the agent's base against the current text; ADR-0006 §4's overlap rules then decide each hunk.

**Reconciliation** — Absorbing a fresh directory listing into the existing tree by matching entries by name, so surviving nodes keep their `NodeId`, expansion state, and loaded children. What makes a watch-triggered re-read non-destructive. ADR-0005 §5.

**Scripted agent** — An `AgentService` that replays a recorded stream with no subprocess, pipes, or timing. It pauses a turn at a file read until the client answers, so a test cannot pretend an agent proposed an edit to a file it never read back. ADR-0007 §7.

**Schema version** — The integer `version` at the top of app-owned TOML. Missing means version 1; older data migrates in memory, newer data loads the fields understood by this build with a diagnostic, and reads never rewrite a file merely to migrate it (ADR-0014 §2).

**Service trait** — The boundary (e.g. `FileSystemService`, `AgentService`) through which widgets and the agent reach the OS. Enables fakes and deterministic tests. ARCHITECTURE.md §7.4.

**Single-owner state** — One task owns all workspace state; everyone else messages it. Keeps human + agent + async events sane. ARCHITECTURE.md §7.1.

**Support tier** — A compatibility evidence level. Tier 1 languages have a built-in recipe plus automated coverage; best-effort languages use the generic LSP path without a shipped recipe or compatibility test. Servers are external and never bundled. See `docs/support.md`.

**TaskAdapter** — A language/tool-specific part of the language-neutral `TaskService`: it supplies structured commands and output decoders for every project kind detected at the workspace root. Shipped adapters include Cargo conventions, `package.json` scripts, pytest, Maven goals, and Gradle tasks; adapters add catalog entries rather than new runners. ADR-0009, ADR-0012, ADR-0013.

**TaskRun** — Model-owned state for one selected catalog task: immutable task specification, human or agent origin, managed terminal, lifecycle status, cancellation state, decoder, and bounded Problems. Human and exact approved ACP invocations use the same representation. ADR-0009.

**Tool window** — A dockable panel (Terminal, Problems, Git, Agent). The Agent pane is a first-class tool window, not a modal popup.

**Terminal session** — The model-owned retained state for one terminal tab: immutable launch specification, owner, title, status, VT screen, bounded output capture, and whether ACP has released its OS resources. Its final screen can outlive the process. ADR-0008 §3.

**Translator** — The I/O-free half of the ACP client: wire messages in, our vocabulary out. Every ACP field name in the codebase lives there and nowhere else, so protocol churn is a diff in one file. ADR-0007 §1.

**Transaction spine** — The transaction/version/rebase machinery that makes multi-actor (human + agent) editing safe.

**WorkspaceSnapshot** — The typed slice of workspace state offered to the agent as context: root, ordered project kinds, the visible tree, and the current selection. Built by a pure function from the same `FileTree` that renders to screen, so the agent's view and the screen cannot disagree. ADR-0005 §7, ADR-0012 §1, ARCHITECTURE.md §9.2.

**Workspace-declared task** — A structured task appended from `[tasks.<id>]` in `.termesh/workspace.toml`. Its `workspace.`-prefixed identity cannot collide with adapter ids, and its program plus argv are never interpreted as a shell string. ADR-0012 §4.

**WorkspaceEdit** — A language-server edit spanning one or more files. Closed targets are loaded asynchronously, every range and supplied wire version is preflighted before mutation, and each file receives one `EditTransaction`/undo group. A read or validation failure abandons the whole pending edit. ADR-0011 §7.
