# Support boundaries for 0.1.0

Termesh 0.1.0 is a public beta. This page states what is exercised, what is expected to
work, and where the current boundary is. It is a compatibility description, not a paid-support
promise.

## Platforms and terminals

The full format, lint, test, and debug-build gate runs in GitHub Actions on Ubuntu, macOS, and
Windows. The release workflow is configured for these artifacts; its credentialed five-target
dispatch is an owner verification step before release:

| Operating system | Architecture | Validation |
|---|---|---|
| Linux (GNU) | x86-64 | Full CI gate; release-workflow target |
| Linux (GNU) | AArch64 | Cross-compiled release-workflow target |
| macOS | x86-64 | Release-workflow target |
| macOS | Apple Silicon | Full CI gate; locally exercised release build; release-workflow target |
| Windows | x86-64 (MSVC) | Builds and passes the unit and integration suite in CI; **PTY teardown unverified — see below**; release-workflow target |

Automated coverage includes Crossterm input, real pseudo-terminal integration tests (on Linux and
macOS — see the Windows note below), recorded VT streams, 60-column snapshots, 16-colour rendering,
and `NO_COLOR`. No named terminal emulator has
a recorded manual certification yet, so 0.1.0 does not claim one. A standards-compatible terminal
that Crossterm supports is expected to work, including over SSH; please report the emulator,
version, operating system, `$TERM`, and colour-related environment variables with terminal bugs.

## Signing

The macOS binaries are signed with a Developer ID Application certificate and notarized by Apple,
with the hardened runtime enabled and a secure timestamp. A download through a browser carries the
quarantine attribute and runs without a Gatekeeper prompt; no `xattr` workaround is needed. Verify
a downloaded copy yourself:

```bash
codesign --verify --strict --verbose=2 ./termesh
codesign --verify --test-requirement="=notarized" ./termesh
```

A standalone executable cannot carry a stapled notarization ticket — stapling targets bundles,
disk images, and installer packages — so Gatekeeper resolves the ticket online the first time the
binary runs. On a machine with no network at that moment, launching may be refused until it can
check once. This is ordinary for a notarized command-line tool.

That also means the `=notarized` check above is an online lookup, and Apple takes a few minutes
after notarizing to serve a new ticket. Running it against a release published moments ago can
report `code failed to satisfy specified code requirement(s)` for a binary that is perfectly
notarized; wait and run it again. The signature check on the line above it is local and answers
immediately either way.

`spctl --assess --type execute` reports `rejected (the code is valid but does not seem to be an
app)` for these binaries. That is `spctl` objecting to a bare executable rather than an app bundle,
not a notarization failure; the `=notarized` requirement above is the check that answers the
question.

The Windows binaries are unsigned. Every release publishes `SHA256SUMS`.

## Language tiers

Language servers are external programs. None is bundled, downloaded, or updated by Termesh.

| Tier | Languages | Required server | Meaning |
|---|---|---|---|
| Tier 1 | Rust | `rust-analyzer` | Built-in recipe plus unit, integration, and UI coverage |
| Tier 1 | TypeScript / JavaScript | `typescript-language-server` | Built-in recipe plus unit, integration, and UI coverage |
| Tier 1 | Python | `pyright-langserver` (from Pyright) | Built-in recipe plus unit, integration, and UI coverage |
| Tier 1 | Java | `jdtls` (Eclipse JDT LS launcher) | Built-in recipe plus unit, integration, and UI coverage |
| Best effort | Other LSP-capable languages | A server configured through the generic LSP path | Protocol path may work, but no recipe or compatibility tests ship in 0.1.0 |

The server executable must be on `PATH`, unless a workspace override supplies an explicit command.
See [language-servers.md](language-servers.md) for installation and override examples.

**Tier 1 describes language-server support, not syntax highlighting.** Highlighting ships one
tree-sitter grammar, Rust, so a TypeScript, Python, or Java file gets diagnostics, navigation,
symbols, rename, and formatting, but is displayed as plain text. Highlighting is cosmetic and the
language-server path is not, which is why the two were allowed to advance separately; additional
grammars are post-beta work.

## Known limitations

- **The Windows binaries are not signed.** SmartScreen may warn on first run, and the warning is
  dismissible. `SHA256SUMS` on each release is the way to confirm you have the file this project
  published. The macOS binaries *are* signed and notarized — see below.
- **Reviewable diffs depend on the agent asking this client to write, and no agent tested so far
  does.** termesh advertises the ACP filesystem capability (`fs/writeTextFile`) and turns an
  agent's write request into a proposal you accept or reject. An agent that edits files directly
  instead has already changed the file by the time you see it, so the review becomes a
  notification rather than a gate, and accepting reports that the buffer already matches. Nothing
  on the client can prevent that. Each of these was checked by recording the ACP session and
  comparing the file on disk before and after:

  | Agent | Adapter | `fs/write_text_file` calls | File changed on disk |
  |---|---|---|---|
  | Codex (in `auto` mode) | `@zed-industries/codex-acp` | 0 | yes — written by the agent |
  | JetBrains Junie | `junie --acp` | 0 | yes — written by the agent |
  | opencode | native ACP | 0 | yes — written by the agent |

  All three send an ACP `diff` content block, which is why the change is visible; it is a display
  payload, not a request to write. The client's review path — proposal, per-hunk accept, rebase on
  a version conflict — is implemented and covered by the protocol and review-loop tests, and it
  engages for any agent that does route writes through the client. None of the ones above do.
- **An agent that offers ACP session modes starts in whichever one it chose**, and termesh does not
  change that for you. Codex opens `read-only` and declines to edit until you move the session with
  `Agent: Session Mode` in the action palette; moving it to `auto` lets Codex edit, but — per the
  table above — it still writes directly, so this buys you a working agent, not a reviewable one.
  The current mode is shown in the Agent pane. Note that `codex-acp` answers `session/set_mode`
  with a bare success and sends no `current_mode_update`, so the pane follows the success reply
  (ADR-0015 §5).
- An ACP agent session does not survive restart. The client restores workspaces, buffers, the
  active tab, pane geometry, terminal working directories, and read-only transcript history, then
  starts a fresh agent session. The implemented ACP baseline has no usable session-load path;
  pretending replay was a resumed session would be unsafe (ADR-0014 §4).
- User-authored `themes/` are deferred. The token layer and compiled dark, 16-colour, and no-colour
  palettes exist, but a theme-file loader would be isolated scope with no `0.1.0` consumer
  (ADR-0014 §1). The `soft_wrap` config key is likewise recognized and diagnosed but not applied;
  correct wrapping requires the editor viewport and decoration model to become wrap-aware.
- A multi-file LSP rename validates the whole edit before mutation, but undo is per file rather
  than one atomic workspace-wide action because each buffer owns its transaction history
  (ADR-0011 §7).
- Completion is explicitly invoked (`Alt+/` by default), not requested after every keystroke. This
  keeps the beta's request and cancellation model deterministic.
- A Java compiler error may appear once from JDT LS and once from `javac`. Deduplicating only by
  path, line, and severity would incorrectly merge distinct same-line diagnostics in other
  languages, so 0.1.0 keeps both rows (ADR-0014 §7).
- **Windows terminals are best-effort in 0.1.0, not Tier 1.** The whole test suite passes on
  Windows except pseudo-terminal teardown, which is unverified: in CI, `child.wait()` on a ConPTY
  child does not return promptly after the process exits, so closing a terminal can report a kill
  timeout (OS error 1460) and the exit event may not arrive. Everything else — editing, search,
  Git, tasks, language servers, and the agent loop — is covered by the same tests as the other
  platforms. The three affected tests are marked `#[ignore]` on Windows rather than deleted, so
  `cargo test -- --ignored` reproduces the behaviour on a Windows host. Diagnosing it needs one;
  no fix is guessed at from CI output.
- There is no debugger, plugin system, or parallel/multi-agent orchestration in 0.1.0. Those are
  post-beta platform work, not partial beta features (ARCHITECTURE §15–§16).

## What beta means

Bug reports and focused compatibility reports are welcome. The configuration schema may change
within the migration contract documented in [configuration.md](configuration.md): known data is
loaded forward where possible, migrations happen in memory, and reading a file never rewrites it.
There is no support response-time or long-term compatibility commitment for 0.1.x.
