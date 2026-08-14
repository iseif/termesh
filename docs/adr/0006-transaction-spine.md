# 6. The transaction spine: ChangeSet representation, rebasing, and undo

Date: 2026-08-07

## Status
Accepted

## Context

Phase 03 is the project's whole bet (CLAUDE.md, ARCHITECTURE.md §16). Its exit criterion —
*type in a file, ask the agent to change it, review its edits as inline diffs, accept, undo* —
rests entirely on the transaction spine described in ARCHITECTURE.md §8. Everything else in the
phase is scaffolding around it.

The Phase-00 stub is a placeholder that cannot do the job, and says so:

```rust
// crates/editor/src/lib.rs
pub struct Change { pub from: usize, pub to: usize, pub insert: String }
pub struct ChangeSet { pub changes: Vec<Change> }

pub fn rebase_onto(&self, _newer: &[EditTransaction]) -> Result<EditTransaction, RebaseError> {
    Err(RebaseError::Conflict)   // stubbed in Phase 00
}
```

Absolute `from`/`to` offsets cannot be mapped through an intervening edit, so this representation
makes rebasing impossible by construction. Replacing it is the first thing Phase 03 does, before
any editor UI sits on top of it — the blast radius today is two crates and one test, and it only
grows.

ARCHITECTURE.md §8 fixes the *shape* (Helix/CodeMirror-6 `ChangeSet`, base-version stamping,
rebase-or-reject) and its "Reuse vs. build" note tells us to borrow Helix's design rather than
depend on its `0.0.0` placeholder crates. What §8 does **not** define is the part that actually decides whether
a user's text survives: it says we rebase "or reject if it no longer composes cleanly" without
saying what *cleanly* means. That single undefined word is the reason for this ADR. ARCHITECTURE.md
§18 names "proposal rebasing against concurrent edits" as a required test class; we cannot write
those tests until the policy below exists.

Two further gaps in the stub, both load-bearing for the exit criterion:

- `EditTransaction` has **no `undo_group`**, though §8 lists one. "Accept, undo" must undo the
  whole accepted proposal in one keystroke, not one hunk — or worse, one insertion — at a time.
- There is no `SelectionMap`, so cursors do not survive a remote edit.

## Decision drivers

- [x] **ChangeSet representation** — positional operation sequence (Helix shape).
- [x] **Rebase mechanism** — position mapping with explicit insert association.
- [x] **Overlap policy** — per-case rules, resolved below. *The core of this ADR.*
- [x] **Rebase timing** — continuous forward-mapping, not deferred to accept.
- [x] **Conflict granularity** — per hunk, never per proposal.
- [x] **Undo model** — linear history, transactions grouped by `UndoGroupId`.

## Options considered — representation

**A. Absolute-offset change list (the current stub).** Simple to read and construct. Rejected:
offsets are meaningless once anything before them moves, so composition and rebasing are not
expressible. This is why the stub's `rebase_onto` is a hardcoded `Err`.

**B. Positional operation sequence — `Retain(n) | Delete(n) | Insert(text)` (Helix/CM6).** Each
changeset is a complete traversal of the document, so it composes with another changeset and can
map any position forward. This is the design ARCHITECTURE.md §8 points at, it is battle-tested in
two mature editors, and it makes rebasing a total function rather than a special case.

**C. Full CRDT.** Rejected explicitly by §8: there is exactly one authoritative buffer and the
agent is asynchronous, so we need lightweight OT, not multiplayer convergence. A CRDT would cost
per-character metadata for a problem we do not have.

We take **B**.

## Decision

### 1. ChangeSet is a positional operation sequence

```rust
pub enum Operation {
    Retain(usize),      // advance n chars unchanged
    Delete(usize),      // drop the next n chars
    Insert(String),     // insert text at the current position
}

pub struct ChangeSet {
    ops: Vec<Operation>,
    len_before: usize,  // document length this applies to
    len_after: usize,   // document length it produces
}
```

Lengths are in **chars**, matching `ropey`'s char indices, not bytes and not graphemes. Rationale:
char offsets are what the rope indexes natively, so no conversion sits on the hot edit path.
Grapheme awareness belongs at the *cursor movement* layer (one arrow press crosses a whole
grapheme cluster), not in the change representation — conflating the two is how editors end up
unable to represent a legitimate edit that splits a cluster. UTF-16 conversion for LSP is a Phase
07 concern and converts at the boundary.

`len_before`/`len_after` are carried, not derived, so composition can assert that changesets line
up. A mismatch is a programming error and panics in debug — it means we tried to apply a change to
a document it was not authored against, which is precisely the class of bug this ADR exists to
make impossible.

Core operations:

- `compose(self, other) -> ChangeSet` — the changeset equivalent to applying both in order.
- `invert(&self, original: &Rope) -> ChangeSet` — the undo. Needs the pre-image because
  `Delete(n)` does not record what it deleted.
- `map_pos(&self, pos: usize, assoc: Assoc) -> usize` — where a position lands afterwards.

### 2. `Assoc` — the tie-break that makes concurrent inserts deterministic

When text is inserted exactly at position `p`, a marker at `p` may land before or after it. This is
not a detail; it is the difference between the agent's insert landing inside the word the human is
typing or after it.

```rust
pub enum Assoc { Before, After }
```

The situation looks symmetric but is not, and the asymmetry is what makes the rule well-defined.
There is exactly one authoritative buffer: human edits are **applied**, proposal hunks are
**pending**. Mapping only ever runs in one direction — pending anchors are mapped through applied
transactions. The agent never maps the human's positions, so "who yields" is never a question of
arrival order.

Rule: **a pending hunk anchor mapped through an applied insertion at the same offset uses
`Assoc::After`.** Agent text lands after text the human already typed there.

This has to hold under §3's continuous rebasing, where the hunk is mapped through every keystroke
individually rather than once. Worked through:

```text
agent proposes Insert("X") anchored at 10
human types "abc" at offset 10, one character at a time:

  'a' → ChangeSet[Retain(10), Insert("a"), …]   anchor 10 --Assoc::After--> 11
  'b' → ChangeSet[Retain(11), Insert("b"), …]   anchor 11 --Assoc::After--> 12
  'c' → ChangeSet[Retain(12), Insert("c"), …]   anchor 12 --Assoc::After--> 13

buffer reads "abc" at 10..13, hunk anchored at 13 — X lands after abc.
```

Stable, and identical to mapping through the single composed changeset. Per-keystroke and batched
rebasing agree, which is the property that lets §3 rebase eagerly without changing outcomes.

### 3. Rebasing is continuous, not deferred to accept

A proposal could be rebased once, at accept time. We reject that: the human reviews hunks *while*
typing, so hunks rendered against a stale base drift away from the code they describe, and a
conflict would only surface at the instant of accepting — the worst possible moment to be told
"never mind."

Instead, every applied transaction maps every live proposal forward:

```text
proposal arrives ──► ProposalState { base_version, original: ChangeSet, hunks: Vec<Hunk> }
                                          │                    │
                        immutable, for re-asking the agent     mapped forward on every
                                                               applied transaction
```

`original` + `base_version` are kept untouched so we can always re-ask the agent with the exact
context it authored against. `hunks` carry current positions, so the review UI is correct by
construction and a conflict is visible *the moment the human creates it*, marked in place.

### 4. Overlap policy — what "composes cleanly" means

Resolved per case. The governing principle: **never silently destroy text the human wrote.** When
in doubt, mark the hunk conflicted and show it — a rejected hunk costs one re-prompt, a
silently-clobbered edit costs trust in the entire product.

| # | Situation | Rule | Why |
|---|---|---|---|
| 1 | Human edit entirely outside the hunk's range | **Rebase.** Map positions, apply. | The common case; nothing is ambiguous. |
| 2 | Human inserted *inside* a range the hunk deletes/replaces | **Conflict the hunk.** | Applying would delete text the human typed without ever showing it to them. Non-negotiable. |
| 3 | Human and hunk insert at the same offset, no deletion | **Rebase**, agent text lands after (§2). | Both intents are preserved; nothing is lost. |
| 4 | Hunk's range straddles a human deletion (anchor partly gone) | **Conflict the hunk.** | The text the agent reasoned about no longer exists; applying would be guesswork. |
| 5 | Human already made the identical change | **Drop the hunk**, mark it satisfied. See the detection rule below. | Not a conflict. Re-applying would duplicate the text. |
| 6 | Human edit adjacent to but not touching the hunk | **Rebase** (case 1 by another name). | Adjacency is not overlap. |

Case 5 is worth calling out: it is the one that stops the loop feeling stupid. Ask an agent to fix a
typo, fix it yourself while it thinks, and the proposal should quietly resolve rather than offering
to make the fix twice.

It is also the one case whose detection does **not** fall out of position mapping, and getting this
wrong inverts its intent. Making the identical change means the human deleted the very span the
hunk was anchored to, so the hunk's mapped range collapses to zero width sitting next to the
human's replacement — which classifies as case 2 or 4 and conflicts, the exact opposite of what we
want. Position mapping cannot distinguish "you deleted my anchor" from "you already did this."

So detection is a **content** check, and it runs *before* conflict classification:

> A hunk that would otherwise be classified conflicted is first tested for satisfaction: let `S` be
> the hunk's post-image (the text it inserts) and `p` its mapped start position. The hunk is
> `Satisfied` if `buffer[p .. p + S.chars().count()] == S`. Otherwise it conflicts as classified.

A window at the mapped position, not the mapped range — the mapped range is precisely the thing the
human's edit destroyed.

This is deliberately **best-effort**, and it fails in the safe direction. A false negative
downgrades to a conflict the human resolves by hand. A false positive requires the human to have
independently produced the agent's exact insertion text at the exact position the hunk mapped to —
in which case dropping the hunk is the right outcome regardless of how we got there.

### 5. Conflict granularity is the hunk

A proposal is a **set of hunks**, each independently rebasable and independently
acceptable/rejectable — ARCHITECTURE.md §9.3 requires per-hunk *and* per-proposal granularity.
A conflict in one hunk never invalidates its siblings. Accepting a proposal with a conflicted hunk
applies the clean ones and leaves the conflict marked, with the agent re-promptable for that hunk
alone.

`RebaseError::Conflict` therefore becomes a per-hunk *state*, not a whole-transaction failure:

```rust
pub enum HunkState { Clean, Conflicted(ConflictReason), Satisfied }
```

`ConflictReason` names the case from the table above, so the UI can say *why* ("you edited inside
this change") instead of a generic failure. Cheap to carry, and it is the difference between a
reviewable tool and a mysterious one.

### 6. Undo — linear history, grouped

```rust
pub struct UndoGroupId(u64);
```

`EditTransaction` gains `undo_group` (§8 specifies it; the stub omits it). The history is a linear
`Vec` of applied transactions paired with their inverses. Undo walks back to the nearest group
boundary and applies inverses in reverse order; redo re-applies.

**The inverse is computed at apply time, not at undo time.** `invert` needs the pre-image (§1:
`Delete(n)` does not record what it deleted), and the only moment the pre-image is guaranteed live
is the instant before the transaction is applied. So `apply` computes the inverse against the
current rope and pushes the pair; undo then never needs a historical document state, and the
history holds no reference to a rope. Reaching for the rope at undo time is the obvious
implementation and it is wrong — by then it is the *post*-image.

Grouping policy:

- Consecutive `Keyboard` insertions coalesce into one group, broken by: a cursor move, a save, an
  edit from a different `EditSource`, or ~300 ms of idle.
- **An accepted proposal is exactly one group**, however many hunks it touched. This is what makes
  the exit criterion's final "undo" mean *undo the agent's change* rather than undo one insertion
  of it.
- `Formatter` and `Lsp` edits each form their own group.

We take a linear history rather than Helix's undo *tree*. The tree is strictly more powerful, but
V1 ships a single cursor and a single authoritative buffer (§10's ship discipline), and a linear
stack over a transaction log is additive to a tree later — the log is the hard part and we are
building it either way.

### 7. Selections transform with the text

`EditTransaction` carries a `SelectionMap` (§8). Applying a transaction maps every range through
`map_pos` — anchor and head independently, `Assoc::After` for the head so typing extends forward
naturally. Cursors therefore survive agent edits, watch-driven reloads, and undo without the model
ever storing a cursor as a raw offset that something else can invalidate.

## Consequences

**The stub is replaced, not extended.** `Change`, `ChangeSet`, and `rebase_onto` in
`crates/editor/src/lib.rs` all change shape. `crates/agent/src/lib.rs`'s `EditProposal` gains hunk
structure. Both are pre-implementation placeholders with one test between them, so the cost is
paid now at its minimum.

**New dependency: `ropey`.** Already declared in `[workspace.dependencies]` (`ropey = "1"`) and
merely unexercised, per ARCHITECTURE.md Appendix A — so this is a newly-*exercised* dependency,
not a new dependency decision. Only `crates/editor` opts in. Note ropey 2.0 is
in beta; we stay on stable 1.x and revisit post-beta, since a rope swap is invisible above the
`Buffer` API.

**Testability is the point.** Every rule in §4 is a unit test with no UI, no filesystem, and no
agent: build a buffer, apply a human transaction, rebase a proposal, assert the outcome. This
discharges ARCHITECTURE.md §18's "proposal rebasing against concurrent edits" class directly, and
the table in §4 *is* the test matrix. Composition and inversion additionally get property tests
(`compose` associativity; `apply` then `invert` round-trips to the original).

**This is the API later phases build on.** LSP document sync (Phase 07) derives its `didChange`
versions from this transaction stream, and tree-sitter (Phase 03, last slice) takes incremental
edits from it. Both were design inputs here: the change stream is public and ordered for exactly
that reason. Changing this representation later means touching both.

**Performance.** `compose` and `map_pos` are O(ops), not O(document). Continuous rebasing (§3)
costs one `map_pos` per live hunk per keystroke — negligible for realistic proposal sizes, and
bounded because proposals are reviewed and dismissed rather than accumulated. If that ever stops
being true, hunk positions can be mapped lazily on render without changing this design.

**What this ADR does not decide.** How proposals *arrive* (ACP wire format), when context is
attached to a turn, and the permission model are all Phase-03 agent concerns and belong to
ADR-0007. This ADR stops at the editor boundary: it defines what a proposal *is* once it exists,
and nothing about where it came from.
