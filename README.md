# termesh

> **0.1.0 — public beta.** The editor and ACP diff-review loop, integrated terminals, search, Git,
> polyglot tasks, crash recovery, session restore, and lazy Rust/TypeScript/Python/Java language
> sessions all share one workspace with the agent. Read the
> [support boundaries](docs/support.md) before you rely on it — they are specific about what is
> tested and what is not.

**A terminal-native, agent-first IDE.** The AI coding agent is a first-class occupant of the workspace — sharing your open buffers, LSP diagnostics, git diff, and terminal output — not a chat box bolted to the side. Built as an [ACP](https://agentclientprotocol.com) client, so any compatible agent (Claude Code, Codex, Gemini CLI, OpenCode, Goose) plugs in. Runs anywhere a terminal runs: SSH, containers, minimal servers.

## Why this exists

The terminal already has great focused tools (Helix, Zellij, Yazi, lazygit) and mature projects that orchestrate them into an IDE-like layout. Re-building *that* adds nothing. Our one reason to exist is the part nobody has built well in the terminal:

- **Agent as a structural peer.** The human and the agent drive the *same* action registry and the *same* transaction-based edit path. Agent edits arrive as **reviewable diffs inline in the editor** — accept, reject, or accept per-hunk.
- **ACP-first, agent-agnostic.** We implement the client side of the open Agent Client Protocol. Bring your own agent and model; we never marry one vendor.
- **Terminal-native and remote-first.** GUI agent IDEs (Zed, JetBrains) can't follow you onto a bare SSH box. This does.

## What it looks like

```text
┌ Project ───────────┬ editor tabs ────────────────────────┬ Agent ─────────────┐
│ ▾ src              │ main.rs •│ app.rs │ README.md        │ ▸ claude-code       │
│   ▾ services       │  13 ~   self.rope.apply(&tx);  ◀diff │ ▎ Proposed 3 edits  │
│      git.rs        │  14 +   self.version += 1;     ◀diff │ ▎ [a]ccept [r]eject │
│   main.rs          │  15   }                              │ …streaming…         │
├────────────────────┼──────────────────────────────────────┤ tool: run cargo test│
│ Outline│Git│Probs   │ Terminal 1 │ Output                   │  ⏵ awaiting approval│
└────────────────────┴──────────────────────────────────────┴─────────────────────┘
 Ctrl+P Files  F9 Search  F10 Actions  F6 Term  F7 Agent  branch: main  0 errors
```

## Agent integration is tiered (so the bet is de-risked)

See [ADR-0003](docs/adr/0003-agent-integration-strategy.md). Two tiers, and the product is useful even if the hard one slips:

- **Tier 0 — terminal CLI (free).** Run any AI CLI inside a managed terminal pane. No agent-specific code at all; it works because the terminal works.
- **Tier 1 — ACP client (the wedge).** Native ACP integration: shared project context, inline diff-review of proposed edits, and permission-gated tool calls.

Both ship in `0.1.0`. The tiering exists so the product stays useful even where the hard one cannot reach.

## Status & roadmap

`0.1.0` is the first public release. Everything described on this page works today; the
[support boundaries](docs/support.md) say where the edges are.

Next, in rough order: parallel and multi-agent sessions, a DAP debugger, a plugin SDK, more
language recipes, and remote/SSH profiles. [ARCHITECTURE.md §16](ARCHITECTURE.md) has the detail,
and [§15](ARCHITECTURE.md) is explicit about what this project is deliberately *not* trying to be.

## Build from source

Requires Rust 1.88 or newer (`Cargo.toml` declares the floor; `rust-toolchain.toml` selects current
stable for contributors).

```bash
git clone https://github.com/iseif/termesh
cd termesh
cargo build --release --locked
./target/release/termesh .              # open a project interactively
./target/release/termesh                # reopen the last project
./target/release/termesh --dump-frame . # headless: print one rendered frame (no TTY)
```

For development, `./scripts/verify.sh` runs formatting, Clippy with warnings denied, the full test
suite, and a workspace build.

## Configuration

Global settings and key overrides live in `config.toml` and `keymap.toml` under
`~/.config/termesh/` on Unix-like systems or `%APPDATA%\termesh\` on Windows. Files overlay the
compiled defaults: a malformed file produces an in-app diagnostic and a usable fallback, never a
failed startup. The command-palette action `config.reload` rereads both files, and F11 shows every
registered action with its current chord.

See [Configuration](docs/configuration.md) for every key, the chord grammar, examples, and the
schema migration contract.

Four panes (Project · Editor · Terminal · Agent), Tab to cycle focus, and Alt+arrows to resize
splits. **Ctrl+P** is Quick Open; **F9** searches workspace text; **F10** opens the action palette.
On a stock Mac keyboard `F9`/`F10` are media keys needing `fn`, so **Alt+F** and **Alt+P** alias the
same two actions — in any terminal that sends Option as Meta (iTerm2, Alacritty, Ghostty, or
Terminal.app with *Use Option as Meta key* enabled). The function keys are the portable guarantee;
the `Alt` chords are the ergonomic route, exactly as `F4`/`Alt+I` already work for the agent prompt.

The **Project** pane is a live file explorer: arrows navigate, Enter/→ expands, ← collapses or
steps to the parent. The tree loads lazily one directory at a time, respects `.gitignore` and
hides dotfiles, and updates itself as files change on disk. New File / New Folder / Rename are in
the palette; Delete is bound to the Delete key and always confirms first.

The **Editor** pane opens a file with Enter, edits it through the transaction spine, saves with
Ctrl+S, and undoes a whole run of typing with Ctrl+Z. Opening and saving go through the filesystem
worker, so a cold file on a network mount never freezes the UI. Rust files are syntax-highlighted,
Ctrl+Tab cycles open files, and Ctrl+F / F3 / Ctrl+H find and replace within the buffer.

Search is project- and language-independent. **Ctrl+P** fuzzy-finds workspace files, while
**F9** searches text across the workspace with a live preview and smart-case matching.
Termesh uses `rg` (ripgrep) as a fast path when available. If it is missing, both searches fall
back to native gitignore-aware implementations, so a fresh installation remains fully usable — same
results, same smart-case rules, same one-result-per-occurrence counting, and an unreadable folder
is skipped rather than failing the search. A test pins the two paths against each other.

Tasks use a language-neutral adapter boundary. Rust contributes Check, Build, Test, and Clippy;
Node workspaces contribute the scripts declared in `package.json` with lockfile-aware npm, pnpm,
Yarn, or Bun selection; Python contributes `pytest`; and `.termesh/workspace.toml` can append exact
structured commands. Press **F5** to choose a task and **Shift+F5** to cancel the newest running
task. Cargo JSON plus common TypeScript, gcc-style, and Python locations become Problems;
**F8** / **Shift+F8** navigate safe paths. See [Tasks and problems](docs/tasks.md).

Git integration runs on its own worker and never blocks rendering. The Project tree and status bar
show cached conflict/staged/modified/untracked state, while **Git: Show Changes** in the action
palette opens conflicts, staged files, and worktree changes as separate groups — and re-reads status
on the way in, so it doubles as the explicit refresh. Press Enter to review the selected staged or
worktree unified diff. Inside Git Changes, `s` stages the selected whole file, `u` unstages it, `c`
opens the commit prompt, and `b` selects an existing local branch. The existing **Ctrl+G** shortcut
stages the selected worktree row; outside the overlay it opens Git Changes and asks you to select a
file.

All eight Git actions are available from **F10 Actions** — type `git` to filter to exactly them:
`git.show`, `git.stage`, `git.unstage`, `git.commit`, `git.branch.checkout`, `git.fetch`,
`git.pull`, and `git.push`. Commit uses the Git index as an exact boundary: it refuses an empty
index or blank message, never runs an implicit `add`, and leaves unstaged edits untouched. Pull is
fast-forward-only, push never forces, and branch checkout is limited to existing local branches.
Advanced history operations and hunk staging remain delegated to terminal Git/lazygit for now, and
an untracked path says so rather than rendering an empty diff.

Language intelligence supports Rust through `rust-analyzer`, TypeScript/JavaScript through
`typescript-language-server`, Python through Pyright, and Java through Eclipse JDT LS. Recipes
resolve together but each server starts only when its first claimed document opens. Live
diagnostics and indexing status merge into Problems and agent context, while
completion/navigation/symbols and transaction-safe rename, quick fixes, and formatting remain
isolated per document owner. Missing servers name their language without disabling siblings. See
[Language servers](docs/language-servers.md) for install commands, virtualenv overrides, status,
logs, actions, and undo behavior.

The **Agent** pane is the point. Configure an ACP agent in `~/.config/termesh/agents.toml`:

```toml
[agents.my-agent]
command = ["some-agent", "--acp"]   # argv, never a shell string
```

Then press **F4** — or **Tab** to the Agent pane and press **Enter** — and type your question.
A session opens by itself. The agent reads your files *through
the editor* — live buffers, unsaved edits included — and its proposed changes arrive as inline
accept/reject hunks marked in the gutter (`~` replaced, `+` added, `!` collided with your own
edit). `a` accepts, `r` rejects, and one Ctrl+Z takes the whole proposal back. Nothing touches disk
until you accept, and a change you have edited inside is flagged rather than applied over.

With no agent configured the editor runs Tier 0 and says so — no vendor is assumed.

The **Terminal** pane runs real interactive shells and commands through managed PTYs. Press
**F6** to focus it (and again to return), or use the palette for `terminal.new`, tab
navigation, restart, close, and copy mode. Normal terminal focus is shell-first: Tab, Ctrl+C,
arrows, and other keys go to the process. **Shift+PageUp** and **Shift+PageDown** scroll back
through output — including while a command is still running, which is how you read the failures
a long test run has already scrolled past. Copy mode provides keyboard selection without sending
input. Exited tabs retain their screen and captured output, and can be restarted.

Tier 0 means any AI CLI can run directly in one of these terminals. Tier 1 ACP agents use the
same terminal service: Termesh shows the exact program, each argv element, cwd, and environment
overrides before execution. **Allow once** starts one command; safe **Allow always** grants are
exact, workspace-scoped, and stored in `.termesh/workspace.toml`. Commands with environment
overrides or a cwd outside the workspace are never persisted as standing grants. Agents receive
the same bounded task catalog as the human; an exact, approved ACP terminal request is tracked as
the same task run, with identical decoded output and Problems behavior.

Agents also receive the model's bounded Git branch/status plus separately labelled staged and
worktree diffs. Agent-proposed Git commands do not bypass the command boundary: they remain standard
ACP `terminal/create` requests with structured program, argv, cwd, and environment, and no process
starts until the human approves the exact command.

See the whole loop without installing anything:

```bash
cargo run --bin termesh -- --dump-frame . --terminal-demo
cargo run --bin termesh -- --dump-frame . --open src/main.rs --agent-demo
cargo run --bin termesh -- --dump-frame . --search-task-demo
cargo run --bin termesh -- --dump-frame --git-demo .
cargo run --bin termesh -- --dump-frame --lsp-demo .
cargo run --bin termesh -- --dump-frame --polyglot-demo .
```

The commands inject recorded terminal output, replay the agent diff-review loop, show a failed
Cargo task with a jumpable Problem, render synthetic Git status/diff state, show language
diagnostics plus hover, and exercise a Rust + TypeScript workspace with discovered npm tasks. They
need no real PTY, ACP agent, Cargo task, `rg`, Git repository, Node, npm, Python, or language server.

## Project layout

One Cargo workspace; widgets and the agent reach the OS only through service traits (never `std::fs`/`std::process` directly). Details in [ARCHITECTURE.md §7](ARCHITECTURE.md).

| Crate | Role |
|---|---|
| `app` | Bootstrap + main loop (the `termesh` binary) |
| `core` | Typed IDs, **action registry**, messages, errors |
| `editor` | Buffers + the **transaction spine** (safe human/agent edits) |
| `agent` | **ACP client**, tiered integration, edit proposals, permissions |
| `ui` `workspace` `filesystem` `syntax` `terminal` `search` `git` `tasks` `lsp` `config` `platform` | Feature domains, each behind a service trait |
| `test-support` | Fixtures, fake services, scripted ACP agent, render snapshots |

## Documentation

- [ARCHITECTURE.md](ARCHITECTURE.md) — the full masterplan (24 sections + appendices)
- [docs/adr/](docs/adr/README.md) — architecture decision records: why the code is shaped this way
- [docs/GLOSSARY.md](docs/GLOSSARY.md) — shared vocabulary
- [docs/language-servers.md](docs/language-servers.md) — Rust, TypeScript/JavaScript, Python, and Java recipes
- [docs/tasks.md](docs/tasks.md) — adapter discovery, workspace tasks, and navigable output
- [docs/configuration.md](docs/configuration.md) — global settings, keymap grammar, and migration
- [docs/support.md](docs/support.md) — tested platforms, language tiers, and beta limitations
- [CHANGELOG.md](CHANGELOG.md) — what shipped in each release
- [CONTRIBUTING.md](CONTRIBUTING.md) · [SECURITY.md](SECURITY.md) · [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md)

## Naming

`termesh` is the project's name as of `0.1.0`. It was picked against the checklist in
[ADR-0004](docs/adr/0004-placeholder-codename.md) — free on GitHub and the package registries, short
to type, and not tied to an existing editor, shell, or terminal. Everything before `0.1.0` was built
under the placeholder codename `termide`, which turned out to be taken.

## License

Dual-licensed under either [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE) at your option.
