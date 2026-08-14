# 5. Workspace filesystem service: concurrency, watching, and ignore semantics

Date: 2026-07-31

## Status
Accepted

## Context
Phase 02 delivers project roots and a live file explorer. This is the **first crate that
touches the real filesystem**, so whatever we choose here becomes the template every later
service (git, tasks, LSP, PTY) copies. Get the shape right.

Constraints that are non-negotiable (see CLAUDE.md invariants + ARCHITECTURE.md §7.4):
- All access sits behind the `FileSystemService` trait. `ui`/widgets never call `std::fs`.
- The render/event loop must never block. Scanning and watching happen off the loop; results
  arrive as messages into the single-owner state.
- The file tree is **agent context** — decide how/when it's exposed to the agent (the whole
  point of the project), even if wiring the agent side lands later.
- Standing stance: **no `tokio` until a phase genuinely needs async** (currently 03/04).

Adding `notify` (watching) and an ignore-rules dependency is itself the load-bearing move that
requires this ADR.

**Starting point (as of Phase 01).** `crates/app/src/main.rs` blocks on `event::read()`, so the
only thing that can ever wake the loop is a keypress. `Model::new()` takes no arguments and holds
no workspace root. `crates/app/src/view.rs` renders a hardcoded `PROJECT_BODY` string. All three
have to change here, and the first one is a concurrency decision, not a detail.

## Decision drivers — resolved below
- [x] **Concurrency model** — Option A: background threads + channels, no `tokio`.
- [x] **Scan strategy** — lazy, per-expanded-directory.
- [x] **Ignore semantics** — the `ignore` crate; ignored entries hidden by default, config toggle.
- [x] **Watching** — `notify` 6 recommended watcher + our own coalescing window in the worker.
- [x] **Edge cases policy** — see "Edge cases" below.
- [x] **Agent exposure** — a typed `WorkspaceSnapshot` built now; wire format deferred to a follow-up ADR.

## Options considered
**A. Background thread(s) + channels, no `tokio`.** Honors the no-async-yet stance; scanning
and `notify` run on a worker thread that emits `AppMessage`s. Simple mental model; proven pattern
in Helix/Zed-style loops. Cost: manual thread/channel plumbing; backpressure is on us.

**B. Introduce `tokio` now.** Uniform async for FS + future services. Cost: pulls the runtime
forward of need, contradicts the current stance, and colors later APIs `async` early. Needs an
explicit reversal of the deferral — call it out here if chosen.

**C. Synchronous + lazy on the loop.** Simplest to write; risks janky UI on large dirs or slow
disks because it can block the render loop — likely violates the no-blocking invariant. Probably
only acceptable for the initial spike, not the shipped design.

## Decision

### 1. Concurrency — Option A, and the event loop inverts
We take **Option A**. CLAUDE.md's standing stance defers `tokio` to Phase 03/04; nothing in
Phase 02 needs a runtime that threads and channels can't serve. Option C is rejected outright:
it violates the no-blocking-the-loop invariant on exactly the case that matters (a cold monorepo
directory on a slow disk). Option B is rejected as premature — when 03/04 introduce `tokio` for
ACP and PTY, the worker here can be rehosted on it without changing the trait (see §3 below).

The load-bearing consequence is that **the main loop stops blocking on the keyboard**. Today
`event::read()` is the only wakeup source; a watcher event arriving from a worker would not
repaint until the user happened to press a key. So:

```text
  crossterm input thread ─┐
                          ├─► mpsc<AppMessage> ─► Model::update ─► view::render(&Model)
  fs worker thread ───────┘
```

- One thread pumps `crossterm::event::read()` and sends `AppMessage::Input(..)`.
- One worker thread owns the `FileSystemService` impl and the watcher, receives
  `FsRequest`s over its own channel, and sends `AppMessage::Fs(..)` back.
- The main loop does a blocking `recv()` on the single `mpsc`, applies the message, redraws.
  It never calls the filesystem and never sleeps on a timer.

This inversion is the template for every later service. `AppMessage` becomes the typed
application-message enum ARCHITECTURE.md §7.1 describes, and lives in `core`.

### 2. Scan strategy — lazy, per expanded directory
ARCHITECTURE.md §16 Phase 02 says "lazy tree" verbatim, and §14 repeats it. Expanding a directory
sends one `FsRequest::ReadDir { node, path }`; the worker reads exactly that one level and returns
its entries. No recursive walk, no background indexing. Collapsed subtrees cost nothing, which is
what makes a monorepo root openable at all.

Consequence: the tree is a `Vec`-arena of nodes keyed by a new `NodeId` typed id, each holding
`Loading | Loaded | Error` state. Per ARCHITECTURE.md §7.3 we key UI identity on `NodeId`, never
on the path — paths change under rename and watch events.

### 3. Trait shape — synchronous trait, asynchrony from the call site
`FileSystemService` methods are **blocking and synchronous**. They are only ever called on the
worker thread, so the concurrency lives in *where* they are called, not in the signature:

```rust
pub trait FileSystemService: Send + Sync {
    fn read_dir(&self, path: &Path) -> Result<Vec<DirEntryInfo>>;
    fn read_file(&self, path: &Path) -> Result<Vec<u8>>;
    fn create_dir(&self, path: &Path) -> Result<()>;
    fn create_file(&self, path: &Path) -> Result<()>;
    fn rename(&self, from: &Path, to: &Path) -> Result<()>;
    fn remove(&self, path: &Path) -> Result<()>;
    fn canonicalize(&self, path: &Path) -> Result<PathBuf>;
}
```

This is deliberate and is **the decision later services copy**. It keeps the trait object-safe,
keeps `test-support`'s in-memory fake trivial (no runtime, no executor), and avoids coloring the
whole codebase `async` before Phase 03 needs it. When `tokio` does arrive, the worker thread
becomes a `spawn_blocking` pool or a dedicated task; the trait does not have to change.

Writes go through the same trait (`file ops` is a Phase 02 exit item), which is what lets the
agent's future `file.create`/`file.rename` tool calls be permission-gated at one chokepoint.

### 4. Ignore semantics — the `ignore` crate, hidden by default
Use **`ignore`** (ripgrep's crate). It honors `.gitignore`, `.ignore`, `.git/info/exclude`, and
global excludes with correct precedence and correct per-directory nesting — hand-rolling this is
a known tar pit, and Phase 05's content search will want the identical matcher so results agree.

For the lazy tree we build the matcher chain once per root and reuse it, reading a single level
per expansion (`WalkBuilder` with depth 1, or `GitignoreBuilder` directly — whichever the API
supports cleanly at the pinned version; the behavior contract is what this ADR fixes, not the call).

Ignored entries are **hidden by default**, with a config toggle to show them dimmed
(ARCHITECTURE.md §13 lists "exclusions" as a config key). Dotfiles are treated the same way:
hidden by default, same toggle. Rationale: the default view should look like the project, and
the agent's context should not be full of `target/` and `node_modules/`.

### 5. Watching — `notify` 6, coalesced by us
Use the `notify` recommended watcher, one per workspace root, recursive, owned by the worker
thread. Raw events are **coalesced in our own code** over a short debounce window
(~100 ms, config-visible later) rather than by pulling in `notify-debouncer-full`:

- One dependency instead of two, and the debouncer's API surface has moved across `notify`
  major versions — we would rather own a small, testable policy than track that.
- Coalescing policy is then unit-testable in `test-support` by feeding synthetic event batches,
  with no OS involved. That is worth more than the code it saves.

Policy: batch events per debounce window, drop events for paths that are ignored or under a
collapsed (unloaded) subtree, collapse create+delete+create storms into a single "this directory
is dirty" marker, and re-read the affected directory level rather than trying to patch the tree
from event deltas. Editor swap-files (`.swp`, `~`, `4913`) are excluded via the ignore matcher.
Reconciling a re-read level against the existing arena preserves `NodeId`s for surviving
entries, so selection and expansion state survive a rename storm.

### 6. Edge cases
- **Symlinks:** not followed for directory traversal (shown, marked, not expanded through).
  A `canonicalize`-based ancestor check guards loops if we later allow following.
- **Permission denied:** the node is marked `Error`, rendered inline, and never panics or aborts
  the expansion of its siblings.
- **Very large directories:** cap the entries materialized per level (e.g. 10k) and append a
  synthetic "… N more" node. Prevents one pathological directory from stalling a render.
- **Non-UTF-8 paths:** stored as `PathBuf`/`OsString`, displayed with `to_string_lossy`. We never
  lose the real path, and never refuse to show a file because of its name.
- **Root detection:** walk up from the CLI argument (default `.`) looking for a VCS/project marker
  (`.git`, `Cargo.toml`); fall back to the given directory itself.

### 7. Agent exposure
No `AgentService` exists yet (Phase 03), so we do not invent a wire format now. What we **do**
build now is the typed source of truth it will read:

```rust
/// The slice of workspace state offered to the agent as context (ARCHITECTURE.md §9.2).
pub struct WorkspaceSnapshot {
    pub root: PathBuf,
    pub project_kind: ProjectKind,
    pub visible_tree: Vec<TreeEntry>,   // loaded nodes only, ignore rules already applied
    pub selection: Option<PathBuf>,
}
```

Built by a pure function from `Model`, so it is snapshot-testable today and cannot drift from
what the human sees — the agent gets *the same* filtered tree, which is the whole premise. The
serialization format and the point in the ACP turn at which it is attached are deferred to the
Phase 03 agent-context ADR.

## Consequences

**Dependencies.** `notify = "6"` is **already declared** in `[workspace.dependencies]` and merely
unused — so it is not a new dependency decision, only a newly-exercised one. The genuinely new
dependency is **`ignore`**, which must be added to `[workspace.dependencies]`. Both are opted into
by `crates/filesystem` only. `ignore` pulls in `globset`/`walkdir`/`regex-automata`; that is
accepted because Phase 05 search needs the same crate, so the cost is shared, not duplicated.

**MSRV.** The workspace pins `rust-version = "1.75"`, itself a leftover of the old sandbox
compiler (CLAUDE.md "Dependency pins to unwind"). Current `ignore` and `notify` releases may
require a newer floor. Resolving this is a **prerequisite check before implementation**: if the
floor must rise, that is its own isolated commit, not something smuggled into this phase.

**Locked-in API shape.** The synchronous-trait-plus-worker-thread pattern in §1 and §3 is now the
template `GitService`, `TaskService`, `PtyService`, and `LanguageService` are expected to follow.
Deviating from it later needs its own ADR.

**Event loop rewrite.** `crates/app/src/main.rs` moves from `event::read()` to an `mpsc` select,
and `AppMessage` lands in `core`. This touches Phase 01 code but does not change its behavior;
the Phase 01 keymap/palette tests must still pass unchanged, which is how we police the claim.

**`test-support` fake.** An in-memory `FakeFileSystem` (tree literal in, `FileSystemService` out,
with injectable errors and synthetic watch batches) ships with this phase. It is what keeps the
`--dump-frame` snapshot deterministic in CI — the headless frame must not depend on the real
filesystem of the machine running it.

**Deferred to follow-up ADRs.** (a) agent context wire format + attachment point (Phase 03);
(b) session persistence file format for recent workspaces — Phase 02 ships the `SessionStore`
trait and an in-memory impl, with on-disk TOML deferred if it proves contentious.
