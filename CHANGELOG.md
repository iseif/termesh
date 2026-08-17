# Changelog

Notable changes to termesh. Versions follow [semantic versioning](https://semver.org); `0.1.x`
is a public beta, so the configuration schema may still change within the migration contract
described in [docs/configuration.md](docs/configuration.md).

## Unreleased

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
