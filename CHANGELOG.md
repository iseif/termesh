# Changelog

Notable changes to termesh. Versions follow [semantic versioning](https://semver.org); `0.1.x`
is a public beta, so the configuration schema may still change within the migration contract
described in [docs/configuration.md](docs/configuration.md).

## 0.1.4 — 2026-08-17

### Added

- **ACP session modes.** An agent that offers modes — Codex opens read-only, with `auto` and
  `full-access` beside it — reports them when the session starts, and the Agent pane shows which one
  is active. `Agent: Session Mode` in the palette lists what the agent offers, in the agent's own
  wording, and switches on request. termesh never changes the mode for you: an agent that defaults
  to read-only is being careful, and overriding that from the client would defeat the reason it did.
  Before this, a mode-aware agent was stuck in whatever it started in and could never be permitted
  to edit (ADR-0015). This makes a read-only agent usable; it does not make its edits reviewable —
  see the support boundaries for which agents route writes through the client.

### Fixed

- The support boundaries, README, and landing page said inline diff review was verified working
  with JetBrains Junie. Recording the ACP session showed otherwise: Junie makes no
  `fs/write_text_file` call and edits the file itself, as do Codex and opencode. All three send an
  ACP `diff` block, which is why the change looks reviewed when it is not. No agent tested so far
  routes writes through the client, and the docs now say so and name what each one did.
- A `session/set_mode` the agent accepted with a bare success and no `current_mode_update` left the
  Agent pane showing the old mode indefinitely. `codex-acp` answers exactly that way, so the pane
  was stranded on `read-only` for the agent session modes exist to unblock. The success reply is
  the agent's own report and now moves the client; a refusal still moves nothing.
- The agent pane rendered a proposal's `[a]ccept`/`[r]eject` prompt, and any pending command
  approval, *above* the transcript. The pane scrolls from the bottom and snaps back there on new
  content, so a long answer pushed the decision off the top and out of view. Both now sit below the
  answer that prompted them.
- Accepting a proposal whose change the buffer already contains reported "nothing applied — every
  hunk conflicted". Nothing had conflicted; the work was already done. It says so now, which
  matters because an agent that writes directly to disk produces exactly this state.
- The turn indicator read `…thinking`, with the ellipsis leading, which looks like a truncated line
  rather than a state.

Worth taking if you use an ACP agent, and worth reading if you were relying on the review loop:
the client's half is implemented and tested, but no agent tried so far — Codex, Junie, opencode —
actually routes its writes through it, so their edits land on disk before you see them. The
[support boundaries](docs/support.md) record what each one did on the wire.

### Install

Prebuilt binaries for Linux (x86-64, AArch64), macOS (Intel, Apple Silicon) and Windows (x86-64)
are attached to this release, with `SHA256SUMS`. The macOS builds are signed and notarized. From
source:

```bash
cargo install termesh --locked
```

Requires Rust 1.88 or newer.

## 0.1.3 — 2026-08-17

### Fixed

- **A `.gitignore` rule naming a directory hid only the directory, not the files under it.**
  The explorer never showed the difference, because it asks about a directory before descending
  and stops there. The file watcher did: it receives deep paths from the OS, so on a Rust project
  every file cargo wrote under `target/` counted as a change and reached the language server as a
  watched-file notification. rust-analyzer then re-analysed, which runs `cargo check`, which
  writes to `target/` again — so it never settled, the status bar cycled through its phases
  indefinitely, and the error count flipped between the real value and zero. Ignored directories
  now hide their contents.
- **The status bar reported `LSP indexing` forever.** Servers report work as a begin/report/end
  stream per token, and every notification was read as "still working" — including the `end` that
  means finished. The indicator now clears when the last outstanding unit of work ends, and still
  shows progress while any remains.

Both are worth taking if you use termesh on a Rust project: before this, rust-analyzer never
reached a steady state and kept re-running `cargo check` in the background.

### Install

Prebuilt binaries for Linux (x86-64, AArch64), macOS (Intel, Apple Silicon) and Windows (x86-64)
are attached to this release, with `SHA256SUMS`. The macOS builds are signed and notarized. From
source:

```bash
cargo install termesh --locked
```

Requires Rust 1.88 or newer.

## 0.1.2 — 2026-08-17

### Fixed

- `termesh --version` printed `termesh 0.1.1 (unknown)` for any binary not built by CI, including
  every `cargo install`. The parenthetical is the commit the build was stamped with, which only a
  release build has; "unknown" read like a fault rather than the ordinary case. A build with no
  commit now prints the version and stops. Release binaries are unchanged and still carry the
  short commit.

### Changed

- **The macOS binaries are now signed with a Developer ID certificate and notarized by Apple**,
  with the hardened runtime and a secure timestamp. A browser download no longer needs
  `xattr -d com.apple.quarantine` — verified by quarantining a build exactly as Safari does and
  running it, against the 0.1.0 binary given the same treatment, which Gatekeeper kills. The
  release workflow signs whenever the credentials are present and now refuses a half-configured
  setup, because signing without notarizing produces a binary Gatekeeper still rejects from a run
  that otherwise looks successful. The Windows binaries remain unsigned.

This is the first release whose macOS artifacts are signed, so upgrading is worthwhile if you
installed from a downloaded archive: 0.1.0 and 0.1.1 shipped unsigned and cannot be re-signed. If
you cleared the quarantine attribute by hand on an earlier download, that is no longer necessary.

### Install

Prebuilt binaries for Linux (x86-64, AArch64), macOS (Intel, Apple Silicon), and Windows (x86-64)
are attached to this release, with `SHA256SUMS`. From source:

```bash
cargo install termesh --locked
```

Requires Rust 1.88 or newer.

## 0.1.1 — 2026-08-17

### Fixed

- The agent pane's empty state — the first screen anyone without an agent configured sees — offered
  the terminal fallback as "Tier 0: run any AI CLI in a terminal instead (Phase 04)". Both are
  build vocabulary: "Phase NN" named the internal plan and "Tier N" named an ADR's framing, and
  neither told the reader what to press. It now says to press F6 and run any AI CLI in a terminal.
  A test scans screen text for that vocabulary so it cannot come back, and the pane's snapshot test
  now asserts the fallback rather than only the setup instructions.

Nothing else changed: no behaviour, no configuration, no dependency. Upgrading from 0.1.0 is
optional, and the [support boundaries](docs/support.md) are unchanged — the macOS binaries are
still unsigned, so a browser download needs `xattr -d com.apple.quarantine ./termesh`.

### Install

Prebuilt binaries for Linux (x86-64, AArch64), macOS (Intel, Apple Silicon), and Windows (x86-64)
are attached to this release, with `SHA256SUMS`. From source:

```bash
cargo install termesh --locked
```

Requires Rust 1.88 or newer.

## 0.1.0 — 2026-08-16

First public release. The whole product ships at once because the parts only mean something
together: the agent is useful because it shares the buffers, the diagnostics, the diff, and the
terminal that the human is already looking at.

### The agent loop

- **ACP client (Tier 1).** Any agent speaking the [Agent Client Protocol](https://agentclientprotocol.com)
  plugs in — Claude Code, Codex, Gemini CLI, OpenCode, Goose — configured as argv in
  `~/.config/termesh/agents.toml`. No vendor is assumed or bundled.
- **Proposed edits arrive as reviewable inline diffs**, marked in the gutter, accepted or rejected
  per hunk. Nothing reaches disk until you accept, a hunk you edited inside is flagged rather than
  overwritten, and one Ctrl+Z takes a whole proposal back.
- **The agent reads through the editor**, so it sees unsaved buffers rather than stale disk.
- **Agent context carries what you see**: bounded git branch and status with separately labelled
  staged and worktree diffs, live diagnostics, terminal output, and the same task catalog.
- **Command execution is permission-gated.** The exact program, argv, cwd, and environment
  overrides are shown before anything runs. "Allow always" grants are exact and workspace-scoped;
  a command with environment overrides or a cwd outside the workspace is never persisted.
- **Tier 0 needs no integration at all** — run any AI CLI in a terminal pane. With no agent
  configured the editor says so instead of assuming one.

### The workspace

- **Editor** on a transaction spine: every change is a versioned transaction, which is what makes
  human and agent edits safe to interleave. Undo groups a run of typing. Rust files are
  syntax-highlighted.
- **File explorer** loading lazily, honouring `.gitignore`, and following changes on disk.
- **Terminals** through managed PTYs — real interactive shells, scrollback while a command is still
  running, keyboard copy mode, and restartable exited tabs.
- **Search**: Ctrl+P fuzzy file open and F9 workspace text search, using `rg` when present and
  falling back to native gitignore-aware implementations that a test pins against it.
- **Git** on its own worker: status, branch, staged and worktree diffs, and explicit-index commits
  that never run an implicit `add`. Pull is fast-forward-only and push never forces.
- **Tasks** through a language-neutral adapter — Cargo, `package.json` scripts with lockfile-aware
  runner selection, `pytest`, Maven/Gradle — plus exact commands from `.termesh/workspace.toml`.
  Failures become navigable Problems.
- **Language servers** for Rust, TypeScript/JavaScript, Python, and Java, started lazily per
  document owner. Diagnostics, navigation, symbols, transaction-safe rename, quick fixes, and
  formatting. Servers are external programs; none is bundled or downloaded.
- **Crash recovery and session restore.** Workspaces, buffers, the active tab, pane geometry, and
  terminal working directories come back. Recovered buffer content is *offered*, never applied
  behind you.
- **Layered configuration** with an in-app diagnostic and a usable fallback for a malformed file —
  never a failed startup.

### Known limitations

Read [docs/support.md](docs/support.md) before relying on this. The boundaries that will affect
people most:

- **The macOS binaries are not signed or notarized.** A browser download is quarantined and
  refused on first launch; clear it with `xattr -d com.apple.quarantine ./termesh`. Installing
  with `cargo install`, `curl`, or `wget` is unaffected.
- **Windows is best-effort, not Tier 1.** Everything passes on Windows except pseudo-terminal
  teardown, which is unverified: closing a terminal can report a kill timeout (OS error 1460) and
  the exit event may not arrive. The three affected tests are marked `#[ignore]` on Windows rather
  than deleted, so a Windows host can reproduce it.
- **Tier 1 means language-server support, not highlighting.** One tree-sitter grammar ships (Rust),
  so a TypeScript, Python, or Java file gets full language intelligence but renders as plain text.
- **An agent session does not survive restart.** Transcript history is restored read-only and a
  fresh session starts; the implemented ACP baseline has no usable session-load path, and
  pretending replay was a resumed session would be unsafe.
- **A multi-file rename undoes per file**, not as one atomic workspace-wide action.
- **Completion is explicitly invoked** (`Alt+/`), not fired after every keystroke.
- **No debugger, plugin system, or multi-agent orchestration** — those are post-beta platform work.
- No terminal emulator has a recorded manual certification yet. A standards-compatible terminal
  Crossterm supports is expected to work, including over SSH.

### Install

Prebuilt binaries for Linux (x86-64, AArch64), macOS (Intel, Apple Silicon), and Windows (x86-64)
are attached to this release, with `SHA256SUMS`. From source:

```bash
cargo install termesh --locked
```

Requires Rust 1.88 or newer.
