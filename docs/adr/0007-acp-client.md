# 7. The ACP client: dependency, transport, context, and the review loop

Date: 2026-08-07

## Status
Accepted

## Context

This ADR covers the half of Phase 03 that justifies the project. ADR-0006 defines what a proposal
*is* once it exists; this one defines where it comes from, how the agent sees the workspace, and
what the human's accept/reject actually does on the wire.

It also discharges a documented debt: ADR-0005's "Deferred to follow-up ADRs" item (a) promised a
Phase-03 ADR for the agent-context wire format and its attachment point. That is §4 below.

ADR-0003 already fixed the strategy — ACP client, agent-agnostic, behind `AgentService`, tiered so
Tier 0 survives if Tier 1 slips. What was left open is every mechanical question, and the answers
turned out to depend on facts about the protocol that we had assumed rather than checked.

**The findings that shaped this ADR.** Read from `agent-client-protocol-schema` 1.6.0 source, not
from memory:

1. The schema crate has **no async dependencies at all** — `serde`, `serde_json`, `serde_with`,
   `schemars`, `strum`, `derive_more`, `anyhow`, optional `tracing`. Pure wire types plus an
   `rpc` module. The full `agent-client-protocol` SDK layers `futures`/`async-io`/`async-process`
   on top; the types underneath do not need any of it.
2. **Edits do not arrive as range edits.** ARCHITECTURE.md §9.3 says "agent edits arrive as
   file/range edits." They do not. They arrive as `ToolCallContent::Diff { path, old_text:
   Option<String>, new_text: String }` — **whole-file before-and-after text**.
3. **`fs/read_text_file` and `fs/write_text_file` are implemented by the *client*, not the agent.**
   The agent asks *us* for file contents. This is the actual mechanism by which an ACP agent sees
   unsaved editor state, and it is a much better fit for this project's thesis than anything we
   would have designed.
4. Permission responses are `AllowOnce | AllowAlways | RejectOnce | RejectAlways` — binary per
   request, with no way to express "I accepted two of your three hunks."
5. Baseline required methods are `session/new`, `session/prompt`, `session/cancel`,
   `session/update`. The SDK is at 2.0.0 with a live `unstable_protocol_v2` feature.

Finding 5 is ADR-0003's spec-churn risk showing up on schedule, and it argues for *more* isolation
behind `AgentService`, not less. Findings 2 and 4 are corrections to ARCHITECTURE.md §9.3 and are
called out as such in Consequences.

## Decision drivers

- [x] **Dependency** — schema types only; we own the transport.
- [x] **Concurrency** — ADR-0005's worker-thread template; no `tokio` in Phase 03.
- [x] **Context assembly** — serve the agent from live buffers via `fs/read_text_file`.
- [x] **Diff → ChangeSet** — derive by diffing; anchor via the version we served.
- [x] **Permissions** — prompt-always default; never advertise a capability we won't honor.
- [x] **Partial accept** — resolved below; the protocol cannot express it directly.

## Decision

### 1. Depend on `agent-client-protocol-schema`, not the full SDK

We take the wire types from upstream and write the JSON-RPC framing ourselves.

**Why not the full SDK.** It brings `futures`, `async-io`, `async-process`, and `blocking` — an
executor and a process runtime — to give us a connection builder we would immediately wrap in our
own trait anyway. Its `Client` API is structured around an async closure owning the session, which
is the inverse of our single-owner-state loop: state lives in `Model`, and services *message* it
(ARCHITECTURE.md §7.1). Adopting it means either inverting our loop or fighting it.

**Why not hand-roll the types.** Spec churn is where the risk actually lives (finding 5). The
types are ~5000 lines of generated-ish serde structs tracking a moving spec; the framing is a few
hundred lines of newline-delimited JSON-RPC that has not changed since JSON-RPC 2.0 in 2010. Take
the volatile part from upstream, own the stable part.

This gets us spec tracking via `cargo update` while leaving the transport under our control, and
it keeps `AgentService` a plain synchronous trait — which is what makes the scripted agent (§7)
possible without an executor.

### 2. Transport — ADR-0005's worker thread, and still no `tokio`

ADR-0005 §1/§3 established the template — blocking service impl on a worker thread, results
returned as `AppMessage` — and said deviating from it needs its own ADR. **We do not deviate.**

```text
  crossterm input thread ─┐
  fs worker thread ───────┼─► mpsc<AppMessage> ─► Model::update ─► view::render(&Model)
  acp worker thread ──────┘
        │
        ├── stdin  ◄── FsRequest-style AgentRequest channel
        └── stdout ──► newline-delimited JSON-RPC ──► AppMessage::Agent(AgentEvent)
```

The agent subprocess is spawned with `std::process::Command` (argv array, never a shell string),
its stdout read by a blocking reader thread that frames newline-delimited JSON-RPC and emits
`AppMessage::Agent(..)`. `AppMessage` gains one variant, exactly as `Fs` was added in Phase 02.

CLAUDE.md's standing "no `tokio` yet" stance therefore **survives Phase 03 intact**. This is worth
stating plainly because CLAUDE.md itself flags 03/04 as the likely moment async arrives: it turns
out ACP does not force it, because one subprocess with two pipes is not a concurrency problem that
needs a reactor. Phase 04's PTY work should revisit the question on its own merits rather than
inheriting a decision from here.

`AgentService` stays synchronous and object-safe, mirroring `FileSystemService`.

### 3. We are the agent's filesystem — this is the shared state

Finding 3 is the most consequential thing in this ADR. ACP clients advertise
`fs/read_text_file` / `fs/write_text_file` capabilities, and the agent then asks the *client* for
file contents rather than reading disk itself.

So when the agent asks for `src/main.rs`, **we answer from the live buffer, including unsaved
changes** — falling back to `FileSystemService` only for files that are not open. That single fact
is ARCHITECTURE.md §9.2's "the editor *is* its context provider" made concrete, and it is precisely
the property a GUI-less, bolt-on integration cannot have. The agent and the human read the same
bytes because there is only one copy.

Capabilities we advertise:

| Capability | Value | Reason |
|---|---|---|
| `fs.readTextFile` | **true** | The whole point. Serves live buffer state. |
| `fs.writeTextFile` | **true** | See below — intercepted into the review loop, never straight to disk. |

`fs/write_text_file` is advertised **true but never applied blind**. Every write becomes an
`EditTransaction` with `source = Agent(ProposalId)` routed through ADR-0006, and reaches disk only
on an explicit save after acceptance.

Advertising `false` was tempting and is wrong: an agent told the client cannot write files does not
give up, it shells out and writes the file itself — moving the edit from a reviewable proposal to
an opaque side effect. Advertising `true` and routing through review is what *keeps* edits in the
loop. Nothing touches disk without acceptance either way; the difference is whether the human sees
the diff.

Reads are scoped to the workspace root; a read outside it is a permission request (§6), not a
silent grant.

### 4. Context assembly and its attachment point

*(This discharges ADR-0005's deferred item (a).)*

We deliberately do **not** invent a bespoke context blob. ACP already has the right shapes, and the
`fs/read_text_file` channel means the agent pulls what it needs rather than us pushing everything.

- **At `session/new`:** the workspace root as `cwd`. That is it.
- **Per `session/prompt` turn:** the existing `WorkspaceSnapshot` (ADR-0005 §7 — project root,
  kind, visible tree with ignore rules already applied, current selection) is rendered as a text
  content block prepended to the turn, plus the open buffer list with dirty markers.
- **On demand:** file contents, via `fs/read_text_file` (§3).

Rationale for pull-over-push: pushing whole buffers into every turn burns context window on files
the agent does not need and goes stale the moment the human types. Pulling is always current by
construction. The snapshot is small, changes rarely, and is what the agent needs to know *what
exists*; the pull channel gives it *what is in there*.

Because `WorkspaceSnapshot` is built from the same `FileTree` the human is looking at, the agent's
view of the project cannot disagree with the screen — the invariant Phase 02 established and
tested, now actually consumed.

Diagnostics (Phase 07) and git diff (Phase 06) attach at the same point when they exist. The
attachment point is the contract; the payload grows.

### 5. Diff → ChangeSet: anchoring by the version we served

Finding 2 means there is a conversion step ARCHITECTURE.md §9.3 did not anticipate. The agent hands
us whole-file `old_text`/`new_text`; ADR-0006 needs a `ChangeSet` against a known `base_version`.

The bridge is §3: because *we* serve the file, we know exactly what the agent read and when.

```text
agent calls fs/read_text_file("src/main.rs")
  └─ we serve buffer text and record (path, BufferId, Version) in the turn's read-set

Diff arrives { path, old_text, new_text }
  ├─ old_text == the text we served at Version V   →  base_version = V.  Clean.
  │                                                   diff(old_text, new_text) → ChangeSet
  │                                                   → hunks → ADR-0006 rebases V → current
  └─ mismatch, or no read-set entry                →  anchor by content (below)
```

The clean path is the normal one, and it lands us exactly where ADR-0006 starts: a `ChangeSet`
stamped with a real version from our own history, rebased forward through whatever the human typed
meanwhile.

The fallback matters because agents may read files by other means (their own shell tools, or a
file we never opened). There, `old_text` is a base that exists in no history of ours. We treat
`old_text` as a synthetic base: diff it against the *current* buffer to test whether it is a
prefix-compatible ancestor, and if the hunk's surrounding context still matches uniquely, anchor
there. If it does not, the hunk is `Conflicted` per ADR-0006 §5 with a reason naming the cause. We
do not guess.

**Diff algorithm:** the `similar` crate (line-level for hunk boundaries, char-level within a hunk).
New workspace dependency, Apache-2.0 (already on `deny.toml`'s allow list), MSRV 1.85, no
transitive weight. It is the same crate that will back the git
diff viewer in Phase 06, so the cost is shared rather than duplicated — the same argument ADR-0005
made for `ignore`.

**Hunk granularity** comes from the line-level diff: each contiguous run of changed lines is one
`Hunk`, which is what the human accepts or rejects individually.

### 6. Permissions — prompt-always, and never advertise what we won't honor

`session/request_permission` carries the tool call being requested. Policy:

- **Default: prompt always.** No implicit grants.
- Scope-based grants (`AllowAlways`) are recorded **per workspace**, in
  `<project>/.termesh/workspace.toml`, never globally and never silently broadened
  (ARCHITECTURE.md §13).
- Commands are shown as **argv arrays** with the working directory, never as an interpolated shell
  string (§9.4, §11).
- Reads outside the workspace root, writes outside it, and network access are each separate
  prompts.

Our `PermissionDecision` enum is realigned to the protocol's four options
(`AllowOnce | AllowAlways | RejectOnce | RejectAlways`); the Phase-00 stub's three-way
`AllowOnce | AllowSession | Deny` cannot round-trip a `RejectAlways` and is replaced.

Actual command *execution* is Phase 04. In Phase 03 an execution request is surfaced and can be
denied, but approving it reports "terminals land in Phase 04" rather than running anything. Better
to render the gate honestly now than to bolt it on later.

### 7. The scripted agent ships with the trait, not after it

`test-support` gains a `ScriptedAgent`: an `AgentService` impl that replays a recorded sequence of
`session/update` notifications — streamed text, tool calls with diffs, permission requests — with
no subprocess, no pipes, and no timing.

It lands **before** the real client, because it is what makes the real client testable, and because
CLAUDE.md's fakes invariant, ARCHITECTURE.md §18, and the README crate table all already promise
it. Every test of the review loop — proposal arrives, human types, hunk rebases, accept applies,
undo reverts — runs against it deterministically.

`--dump-frame` is extended to drive a scripted agent so **diff-hunk rendering is snapshot-tested
headlessly in CI**. That is the gate's "headless smoke where it applies," and it is the
machine-checkable proof that the phase's exit criterion actually works.

### 8. Partial accept — the one place the protocol cannot express our UX

Finding 4: permission responses are binary, but ADR-0006 §5 makes per-hunk acceptance a first-class
outcome. There is no `AllowPartial`.

Decision:

- **All hunks accepted** → `AllowOnce` (or `AllowAlways` if the human chose to remember).
- **All rejected** → `RejectOnce`.
- **Some accepted** → `RejectOnce`, **plus** an automatically composed follow-up message on the
  next turn stating exactly which hunks were applied and which were declined.

Responding `RejectOnce` for a partial accept looks wrong and is right: from the agent's
perspective its proposed write did not happen *as proposed*. Claiming `AllowOnce` would leave the
agent believing the file matches `new_text` when it does not — and the next thing it does is build
on that belief. The follow-up message plus a fresh `fs/read_text_file` resyncs it to the truth,
which is the version the human actually approved.

## Consequences

**ARCHITECTURE.md needs two corrections.** §9.3's "agent edits arrive as file/range edits" is
wrong (finding 2) and §9.4's permission vocabulary predates the protocol's four options
(finding 4). Both get fixed when this ADR is accepted — the masterplan outranks this file
(CLAUDE.md authority order), so it is the masterplan that must be corrected, not quietly worked
around.

**New dependencies:** `agent-client-protocol-schema` (Apache-2.0), `similar` (Apache-2.0),
`serde_json` (already transitively present). Both licenses are on `deny.toml`'s allow list. All opted into by `crates/agent` only, except `similar` which
`crates/editor` also uses for hunk construction. No executor, no runtime.

**`AgentService` grows past the Phase-00 stub** — capability negotiation, the client-side
`fs/read_text_file` handler, and the read-set that §5 anchors on. The stub's `NullAgent` stays as
the Tier 0 default so a user with no agent configured still gets a working editor.

**Agent configuration is data, not code.** `~/.config/termesh/agents.toml` holds agent commands as
argv arrays (ARCHITECTURE.md §13). Ship recipes for several agents; default to none configured
rather than to a vendor. ADR-0003's agent-agnosticism is a promise we keep in the *default*, not
just in the abstraction.

**Spec churn is now a live maintenance cost, not a hypothetical.** Pinning the schema crate means
`cargo update` can surface breaking protocol changes. The scripted agent's recorded fixtures are
what turn that from a debugging session into a failing test, which is exactly the mitigation
ADR-0003 promised.

**What this ADR does not decide.** How hunks are *rendered* (decoration layout, gutter markers,
colors) is settled in the phase's decoration slice, not here — it is UI, it is reversible, and it
does not deserve an ADR. Multi-session/parallel agents remain out of scope (§9.5, Phase 09).
