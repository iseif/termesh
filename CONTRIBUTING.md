# Contributing to termesh

Thanks for your interest — this is a young, opinionated project and early contributors shape it a lot.

## Before large changes: open an RFC
Anything touching the **action registry**, the **transaction spine** (`editor`), or the **ACP client** (`agent`) needs an RFC / ADR first (see `docs/adr/`). These are load-bearing; the agent integration hardens exactly these boundaries, so we keep them deliberate.

## Dev setup
```bash
cargo build --workspace
cargo test  --workspace
cargo fmt   --all
cargo clippy --workspace --all-targets -- -D warnings
```
CI runs the same on Linux, macOS, and Windows. Keep the tree `fmt`-clean and clippy-clean.

## Invariants

These are not style preferences. Each one is load-bearing for the thing that makes this project
different from a terminal multiplexer with an editor in it, and comments throughout the tree cite
this section by name. Breaking one needs an ADR, not a review comment.

- **Service boundaries.** Widgets and the agent reach the OS only through service traits
  (`FileSystemService`, `GitService`, `PtyService`, `LanguageService`, …). Never call `std::fs`,
  `std::process`, a PTY, or an LSP directly from `ui` or a feature widget (ARCHITECTURE.md §7.4).
  This boundary is what makes the whole system testable against in-memory fakes.
- **Transaction spine.** Every buffer change is an `EditTransaction` stamped with a base version.
  No direct rope mutation. Agent edits are `ChangeSet`s applied-or-rebased on accept — **never
  written blind**. This is what lets a human and an agent edit the same buffer safely.
- **Single-owner state, pure view.** One owner of application state; `view::render(&Model)` stays a
  pure function of that state. No blocking I/O on the render or event loop — agent streaming must
  never stall typing.
- **One command surface.** The action registry backs both the keymap and the palette. New
  user-invocable behaviour becomes an `Action`/`Command`, not a special-case keybinding. The
  registry is *not* an agent tool surface: stable ACP has no portable client-owned custom-tool
  registration ([ADR-0009](docs/adr/0009-search-task-execution-and-acp-semantics.md)).
- **ACP stays isolated.** All ACP wire types live behind `AgentService` in the `agent` crate. They
  must not leak into `editor` or `ui`.
- **Does the agent get to see this?** Whenever you add context — file tree, diagnostics, git diff,
  terminal output — ask whether the agent sees it too, and wire it into agent context. The agent
  reading the same state the human reads is the entire premise.
- **Fakes for services.** Every new service ships with a fake or scripted implementation in
  `test-support`, so logic is testable without touching the OS.

## Other ground rules
- New logic gets unit tests; new UI surfaces get a `TestBackend` snapshot assertion.
- UI PRs include a screenshot or asciinema recording.

## Good first issues
Look for `good first issue` and `help wanted` labels. Each core crate has a module owner listed in its `lib.rs` header over time.

## Commits & PRs
Conventional-ish commit subjects (`feat:`, `fix:`, `docs:`, `refactor:`). Squash-merge to `main`.

`./scripts/verify.sh` runs the whole gate — formatting, Clippy with warnings denied, the test
suite, and a workspace build — in one command. It is what CI runs, so a green local run means a
green PR.
