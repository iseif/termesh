# 14. Beta hardening: layered config, offered-not-applied crash recovery, and a published ACP session-restore boundary

Date: 2026-08-12

## Status

Accepted

## Context

Phase 10 turns nine phases of working features into a `0.1.0` beta a stranger can install,
configure, and trust, per ARCHITECTURE §16's exit criterion: *"`0.1.0` beta with clear support
boundaries."* §16's own list of work — crash recovery, perf profiling, large-project tests,
low-colour/narrow-SSH testing, keymap conflict review, onboarding, config migration, signed
binaries, packaging, docs site, demo recording — mixes three kinds of work that cannot be executed
the same way, and untangling that mixture is the one load-bearing judgment this ADR exists to
record before implementation starts:

- **Ordinary Rust work behind existing service boundaries** — configuration, session restore,
  crash recovery, colour/width degradation, help, instrumentation, large-workspace behaviour,
  release metadata. Testable, reversible, this phase's job.
- **Irreversible or credentialed release execution** — registry/package-manager reservations,
  certificate provisioning, secrets, the tag, the publish, the demo, a hosted docs site. No
  implementation agent has the credentials or the authority for these, and several cannot be
  undone. These become `docs/release-checklist-0.1.0.md`: exact commands, executed by the project
  owner. **No implementation agent runs `cargo publish`, `git tag`, or `git push --tags`.**
- **Deferred, with the reason recorded** — `~/.config/<app>/themes/` (§13) and a hosted
  documentation site (§16), both partial deviations from the referenced sections rather than
  silent gaps.

**Findings, read from the code rather than recalled.** Each cites the file and line it came from,
so it can be checked against the tree at the commit this ADR was written:

1. §13 promises five configuration files. Two exist: `~/.config/termesh/agents.toml`
   (`crates/config/src/agents.rs`) and `<project>/.termesh/workspace.toml`
   (`crates/workspace/src/permissions.rs`). Three do not: `config.toml`, `keymap.toml`, `themes/`.
   `crates/config/src/lib.rs:1-2` has said "TOML loading layers on top later" since Phase 01; it
   never did. §16's "config migration" is vacuous until a schema exists to version.
2. `crates/workspace/src/session.rs` persists exactly `recent: Vec<PathBuf>`. Its own header says
   open files, pane sizes, terminal cwd, and the agent session "join this as those phases add
   them." They never did.
3. **Agent-session restore is an ACP fact, not a serialization gap.** ADR-0007 §5 fixes the
   baseline required methods at `session/new`, `session/prompt`, `session/cancel`,
   `session/update`. There is no `session/load` in this client, and the `Pending::Initialize` arm
   in `crates/agent/src/protocol.rs:341-347` discards the `initialize` result wholesale — it
   drains the queue and nothing else. `agentCapabilities` is never read, so the client cannot
   resume a session and cannot even discover whether the agent it is talking to would support
   resuming one. §23 item 10 — *"close and later restore the workspace, including the agent
   session"* — cannot be honoured as written.
4. `Model::serve_read` (`crates/app/src/model.rs:3498-3522`) returns `contents: None` for any path
   not already open in a buffer, on a comment claiming "the worker reads it" — nothing does.
   `crates/agent/src/protocol.rs:206-214` turns that `None` into a JSON-RPC `-32000` error, while
   `initialize` advertises `readTextFile: true` at `protocol.rs:117`. The client tells every agent
   it can read files and then refuses most of them. This is a carried-over defect, flagged
   out-of-scope during Phase 07 and fixed in this phase's Task 1 — first, because everything else
   the agent is given to look at is only as useful as its ability to then read it.
5. `crates/ui/src/theme.rs:33` sets `statusbar_bg: Color::Indexed(236)` on a status bar that is
   always on screen. Neither `NO_COLOR` nor `COLORTERM` is read anywhere in the workspace.
6. `tracing` sits in `[workspace.dependencies]` and is used by nobody; `crates/agent/src/acp.rs:200`
   is `fn tracing_line(_line: &str) {}`.
7. The terminal-side panic hook (`crates/app/src/tui.rs:37-43`) is done and correct — it always
   restores the terminal. What is unproven is that a panic unwind actually reaches the ten `Drop`
   impls across the worker crates, and a dirty buffer at panic time is gone today.
8. `LICENSE-APACHE` is a 22-line stub, `SECURITY.md:4` and `CODE_OF_CONDUCT.md:5` carry `TODO`
   addresses, `Cargo.toml`'s `repository` is a placeholder, `version = "0.0.0"`, there is no
   `--version` flag, and `Cargo.toml:9`'s MSRV comment cites `agent-client-protocol-schema`, which
   is in neither `crates/agent/Cargo.toml` nor `Cargo.lock` (ADR-0007 §1 hand-rolls the wire types
   instead of depending on the SDK that would have carried it).
9. `crates/config/src/lib.rs:263` guards default bindings against chords the terminal cannot
   deliver, but nothing guards against two defaults claiming the same chord in the same
   `KeyContext` — the map is a `HashMap`, and `insert` silently overwrites. `terminal.copy_mode` is
   also still palette-only and unreachable from a terminal that has focus and swallows the palette
   key, noted during the Phase 09 terminal-scroll fix.
10. The diagnostics merge in `Model::problem_rows` (`crates/app/src/model.rs:2477-2510`) dedups on
    `(path, line, normalize_problem_message(message))`. Eclipse JDT LS and `javac` word the same
    underlying error differently, so for Java this key never coincides and both rows survive.

This ADR is required before implementation begins for two independent reasons under ADR-0001:
finding 3 is ACP semantics, and decision 7 below adds three actions to the registry.

## Decision drivers

- Nothing irreversible ships from this phase; every credentialed or outward-facing step is a
  checklist item for the owner, never a command run by the implementing agent.
- A malformed or absent configuration file degrades to compiled defaults plus a diagnostic. It
  never prevents startup.
- Every degradation states, in the app, which fallback it took (§13).
- The transaction spine, `FileSystemService` boundary, and ADR-0009's "context, not client-owned
  tools" all hold unchanged; this phase corrects and extends, it does not redesign.
- Wall-clock assertions stay out of `cargo test`; only algorithmic properties gate the build.

## Decision

### 1. The `0.1.0` configuration surface is `config.toml` + `keymap.toml`; `themes/` is deferred

**Options considered.** (A) Ship exactly the two files that have real consumers this phase and
defer `themes/` — chosen. (B) Ship all three files §13 names, including a `themes/` loader. (C)
Ship no configuration surface this phase and defer all of it to Phase 11.

(B) was rejected because the theme *token* layer already exists (`crates/ui/src/theme.rs`) and a
`themes/` directory loader has no other work to piggyback on in this phase — it would be built,
tested, and shipped with nothing else in the ladder depending on it. (C) was rejected because it
leaves §16's "config migration" permanently vacuous: there is no schema to migrate without a
schema to version, and a beta IDE that cannot rebind a key generates issues on day one.

**Decision:** ship `config.toml` (global settings) and `keymap.toml` (user keybindings) this
phase. Defer `~/.config/<app>/themes/` to Phase 11. This is a **partial deviation from §13**,
recorded here and in `docs/support.md` (Task 15), with the reason above — the deferral costs a
file reader later, not a redesign now.

**Also deferred, decided during Task 3 implementation:** `config.toml`'s `soft_wrap` key is parsed
and round-trips, but is not applied this phase. The editor's cursor overlay is drawn manually in
`crates/ui/src/widgets.rs` on the assumption that one logical line is one physical row; honouring
`soft_wrap` correctly means `scroll_top`, `clamp_viewport`, the gutter, and the decoration spans
all become wrap-aware, which is a rendering rewrite on the most snapshot-tested path in the app,
not a slice of this task. Confirmed with the project owner before deferring. `tab_width`, `shell`,
and `exclusions` all reached real consumers in Task 3 as planned; only `soft_wrap` moved to Phase
11, alongside `themes/`.

To keep this from reading as a contradiction against the deferral: `config.toml`'s `theme` *key*
ships this phase (Task 3) and selects among the compiled palettes / the colour-depth override that
Task 7 adds. What is deferred is the `themes/` *directory* of user-authored theme files, which has
no key and no loader until Phase 11.

### 2. The migration contract

A `version` integer key sits at the top of every file this project owns: `config.toml`,
`keymap.toml`, `session.toml`. Absent `version` means version 1 — the schema predates the key that
names it, so this is not an error. A file at the current version loads as-is. A file from an
*older* version is migrated in memory on read and is rewritten only when the app next writes that
file for another reason — never as a side effect of starting, because rewriting a user's config on
every upgrade is how comments and formatting disappear. A file from a *newer* version loads
whatever it understands and reports what it ignored, rather than refusing to start. Unknown keys
at the current version are **preserved and reported, never dropped** — silently dropping a key is
how a user spends an hour on a typo.

No alternative was seriously considered here: every other option (refuse to start on
version mismatch, silently drop unknown data, rewrite on every read) directly contradicts the
"malformed config never blocks startup" driver above.

### 3. Configuration layers over the compiled defaults, never replaces them

**Options considered.** (A) The file overlays the compiled defaults by key — chosen. (B) Presence
of the file replaces the compiled defaults wholesale.

(B) turns one typo into an unusable editor: a single malformed line in a hand-edited
`keymap.toml` would cost the user all forty-seven bindings, not one. (A) means
`config::default_keymap()` remains the source of truth; `keymap.toml` and `config.toml` overlay
named keys onto it, a user who rebinds one chord keeps the other forty-six, and a parse failure
falls back to a fully usable editor plus a `ConfigDiagnostic` naming the file, the line, the
problem, and the fallback taken.

### 4. ACP session restoration is impossible today, and §23 item 10 degrades in public

**Options considered.**
(A) Restore workspace state (roots, buffers, active tab, pane geometry, terminal cwds), start a
**fresh** agent session, and keep the prior transcript as read-only history — chosen.
(B) Silently start a new session and discard the prior transcript with no indication anything was
lost.
(C) Fake a "resumed" session by replaying the stored transcript back into a new session's prompt
context, presenting it to the user as if it had continued.
(D) Block Phase 10 — and therefore the beta — until upstream ACP ships `session/load` and this
client implements it.

(B) fails the exit criterion directly: "clear support boundaries" means the boundary is stated,
not silently absorbed. (C) is worse than (B), not better — it manufactures the *appearance* of
continuity the protocol cannot back, and an agent acting on a replayed-but-not-actually-resumed
context is a correctness hazard, not a convenience. (D) is rejected because it makes the beta
hostage to a protocol capability with no committed timeline, over one line item out of fifteen
exit criteria; ADR-0003's tiered strategy exists precisely so a single ACP gap does not block
everything behind it.

**Decision:** reopening a workspace restores the roots, the previously open buffers, the active
tab, the pane geometry, and the terminal working directories. It starts a **fresh** agent session.
The prior transcript is retained and shown as read-only history so context is not simply
discarded, and the agent pane states once, plainly, that the session did not resume. This is a
published support boundary (`docs/support.md`, ADR-0014 §4 cited from there), not a skipped
checkbox.

**Also decided:** the client begins **reading** `agentCapabilities` from the `initialize` result
(`AgentEvent::Ready { capabilities }`, parsed in the `Pending::Initialize` arm before the existing
queue drain, which must be preserved verbatim). Nothing is gated on the flag this phase — Phase 11
does that. This is added now because advertising a capability we cannot honour is exactly the
failure mode `protocol.rs:499-503` already warns against for `fs/write_text_file`, and because
Phase 11's multi-agent work cannot start without the parsing existing first.

This is ACP semantics touched directly, which is why this ADR exists and why Task 1 onward does
not begin until it is accepted.

### 5. The crash-recovery model: drafts are offered, never applied

**Options considered.** (A) Mirror dirty buffers to draft files; on restore, offer them through a
modal and let the human choose restore-all, restore-selected, or discard — chosen. (B)
Auto-restore drafts silently when the workspace reopens.

(B) is rejected: it silently overwrites the wrong thing exactly when the user is least able to
notice — the file on disk may have changed since the crash (another tool, another branch, another
person), and applying stale text over current text without asking is a worse failure mode than
losing the draft would have been.

**Decision:** a dirty buffer is mirrored, debounced, to a draft file under the config directory,
one per buffer, named by a hash of the absolute path (so `/a/src/main.rs` and `/b/src/main.rs`
cannot collide) with the basename kept readable. Cleared on save. Reaped past a retention window
at startup. On opening a workspace with drafts present, the app offers them; accepting applies
each one as an `EditTransaction` — undoable, never a blind write, which is the transaction-spine
invariant that makes offering safe. Discarding deletes the draft files; ignoring the prompt leaves
them for next time.

Proving the other half of crash recovery — that the ten `Drop` impls across the worker crates
actually run on a panic unwind, so no `rust-analyzer`, `jdtls`, or PTY child survives a crash — is
Task 10's teardown test, not a design choice; it is asserted, not decided.

### 6. Support boundaries are a published contract, not a README aside

`docs/support.md` states the platforms and terminals actually exercised by CI versus expected to
work, language support tiers (Tier 1 = recipe plus tests; best-effort = generic LSP path,
untested), the known-limitations list — including the agent-session boundary from decision 4 above
— and what "beta" means here (bug reports welcome, config format may change within the migration
contract of decision 2, no support commitment). The phase's exit criterion is *"`0.1.0` beta with
clear support boundaries,"* so this document is the deliverable the criterion names, not a summary
written after the fact.

### 7. Three actions join the registry; the registry moves from 47 to 50

Per ADR-0001, a registry change requires this ADR. Three actions are added:

| id | title | agent-gated? | why |
|---|---|---|---|
| `help.show` | `Help: Keys and Actions` | No | Opens a read-only overlay; nothing mutates. Lands in Task 8. |
| `config.reload` | `Config: Reload` | No | Re-reads `config.toml`/`keymap.toml` from disk and re-layers over compiled defaults; no buffer edit, no process start — the same "reads are free" rule already applied to `Action::LspHover` and family. **Lands in Task 5** — the plan's Task 5 text does not currently mention it and is widened by this ADR to add it, since it is the task that already owns the config read path end to end and a config surface with no way to reload it without a restart is exactly the day-one issue report this phase exists to prevent. |
| `workspace.restore_drafts` | `Workspace: Restore Drafts` | Yes | Applies draft text into live buffers via `EditTransaction`; the existing rule is that anything editing a buffer or starting a process is gated. Lands in Task 10. |

`assert_eq!(reg.len(), 47)` at `crates/core/src/lib.rs:498` becomes `assert_eq!(reg.len(), 50)`
across Tasks 5, 8, and 10 as each action lands, so the implementation and the test agree on the
number decided here rather than discovering it by trial.

**The JDT-vs-javac duplicate diagnostic (finding 10).** Two options: (A) keep the dedup key as
`(path, line, normalized message)` and accept the duplication for Java, documented in
`docs/support.md` as a known limitation — chosen. (B) change the key to `(path, line, severity)`
with the live server's row winning over the task row.

(B) is rejected for this phase: the key in `Model::problem_rows` is shared by every language, not
only Java, and loosening it to drop the message would silently merge two *genuinely different*
diagnostics that happen to land on the same line and severity in any language — a correctness
regression with a larger blast radius than the cosmetic duplication it would fix. (A) costs one
documented line in `docs/support.md` and changes no shared code. Reconsider only if a concrete
report shows the merged-away case actually happening for a language other than the one motivating
it.

## Consequences

**The phase has a stable shape.** Bucket A (this ADR's decisions 1, 2, 3, 5, 7, and the read-path
fix) is ordinary, testable Rust work behind existing service boundaries. Bucket B is fully
specified as `docs/release-checklist-0.1.0.md`, not merely deferred. Bucket C's two deferrals
(`themes/`, hosted docs) are recorded against §13 and §16 respectively, with reasons, not silently
dropped.

**§23 item 10 is amended in public, not silently met.** ARCHITECTURE §23 gets a cited boundary
note rather than a quiet edit to match the code — a reader of §23 must be able to find out, from
the document itself, that agent-session restore does not happen, and why.

**Configuration becomes a compatibility promise.** Once a user has a `config.toml`, every future
schema change is a migration under decision 2. The mitigation is shipping the smallest surface
§13 actually names — nothing more — and making `version` load-bearing from `0.1.0` rather than
retrofitted later.

**The registry grows by three, predictably.** `crates/core/src/lib.rs`'s `Action` enum, `id()`,
`title()`, `agent_needs_permission()`, `with_defaults()`, and the `reg.len()` assertion all move
together in the same commit that adds each action (Tasks 5, 8, and 10), per the table above.

**Nothing here authorizes Task 16.** The rename is gated on the project owner supplying the
released name (ARCHITECTURE §21, ADR-0004) and is out of scope for this ADR's decisions; it is its
own commit, after Task 15. The owner has already supplied the name — **Termesh** — during this
phase's kickoff; Task 16 still does not start until Task 15 is complete, per the plan's Global
Constraints.

**What remains explicitly excluded**, all owner-executed and living in
`docs/release-checklist-0.1.0.md`: reserving the name on GitHub/crates.io/Homebrew/Scoop/WinGet,
provisioning signing certificates, adding secrets, recording the demo, standing up a hosted docs
site, and cutting the tag. No implementation agent runs `cargo publish`, `git tag`, or
`git push --tags`.
