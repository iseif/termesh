# 12. Polyglot workspaces detect every root language and start sessions lazily

Date: 2026-08-11

## Status
Accepted

## Context

Phase 07 established Termesh's language-server boundary with one Rust recipe, while deliberately
building the seams needed for more than one server: sessions are keyed by `LspServerId`, documents
are routed by extension, `LspState` owns monotonic wire versions, live buffer text drives document
synchronization, server edits re-enter the transaction spine, and every inbound server request is
answered. ADR-0011 fixed those properties. Phase 08 must exercise them with TypeScript/JavaScript
and Python without weakening any of them, broadening `crates/lsp` beyond its complete dependency
set of `termesh-core` and `serde_json`, or requiring a toolchain to be installed for tests.

The roadmap originally placed additional language recipes after public beta hardening. The project
owner inverted that sequence: polyglot TypeScript/JavaScript and Python workspaces are Phase 08,
Java is Phase 09, public beta hardening moves to Phase 10, and post-beta platform features move to
Phase 11. ARCHITECTURE.md Section 16 already reflects that order and points here for the reasoning.
The polyglot seam from Phase 07 is still unexercised and is cheaper to validate while its design is
fresh. Hardening is also partly a function of how many servers run; profiling, recovery, and
large-project testing a one-server product first would have to be repeated once several servers
were active.

Java is deferred to Phase 09 because Eclipse JDT LS is materially more expensive than a recipe
row: it needs launcher discovery, platform-specific configuration, per-workspace data, JVM flags,
initialization settings, and integration with two different build systems. Go and Dart are
excluded by choice rather than oversight. A Go project remains detectable and fully usable as an
editor even though it has no Phase 08 language-server or task recipe.

The phase also changes two model-facing contracts. Project detection must return every kind found
at a workspace root so both task discovery and agent context share one source of truth. Task
discovery must read project configuration through `FileSystemService`, which changes the
`TaskService` trait signature. Finally, the eager session lifecycle established for the single
Phase 07 recipe must become lazy so opening a polyglot repository does not start every possible
server before the developer opens a claimed document. These are load-bearing choices rather than
implementation details.

## Options considered

### A. Detect a set of root kinds, start sessions lazily, and discover tasks from configuration

Report every marker found at the chosen root, resolve recipes without launching them, and start a
session only when a document it claims opens. Read task declarations synchronously from project
configuration through `FileSystemService`. This keeps startup proportional to the languages the
developer actually edits and makes `F5` describe the repository rather than a guessed convention.

### B. Detect a set of root kinds and start every session eagerly

Resolving and launching every recipe at workspace open produces a simpler lifecycle and may make
diagnostics available sooner. It also makes a polyglot repository spawn every configured process
to edit one file, which is especially costly over SSH or on a modest machine and grows worse when
Java arrives.

### C. Keep one project kind per workspace and add more recipes

This is the smallest code change, but it cannot deliver the motivating case. A frontend beside a
backend still resolves only one language, task discovery remains incomplete, and the multi-session
routing seam from Phase 07 remains untested.

### D. Discover tasks asynchronously by invoking build tools

A worker could enumerate npm workspaces or ask Gradle for its real task graph. That adds process
execution, asynchronous state, and a catalog that changes after workspace open. Phase 08's required
Node scripts are already declared in `package.json`, so that cost is not justified yet.

We take Option A.

## Decision

### 1. Detection reports a set of kinds, at the root only

`WorkspaceRoot` keeps `kind` as the primary answer for display and existing single-kind call sites
and gains `kinds` containing every detected project kind in marker-priority order. When the set is
non-empty, `kind == kinds[0]`; a root with no project marker keeps `kind == Unknown` and an empty
`kinds`. `WorkspaceSnapshot` carries the same set so agent context and human-facing project state
do not derive languages independently.

Detection remains a scan of the selected root. A marker in a nested directory is not detected.
Finding project sub-roots requires monorepo ownership and routing rules: "detect a set at the root"
is bounded, while "find every project in the tree" is not. Nested project detection is explicitly
out of scope for Phase 08.

### 2. Sessions start lazily on the first claimed document

A language recipe is resolved at workspace open and retained as configuration without starting a
process. `LspLoadState::Idle`, declared but unused in Phase 07, represents this configured state.
When `sync_lsp_documents` sees an open path that no live session owns, it starts the configured
recipe that claims the path, creates the session keyed by a new `LspServerId`, and sends `didOpen`
to that session. A document no recipe claims starts nothing.

This prevents a polyglot repository from spawning a JVM, a Node process, and rust-analyzer merely
to edit one file, preserving Termesh's ability to run where the code lives. The accepted cost is
that a missing or misconfigured server fails at first use rather than workspace open. That failure
must surface immediately and name the affected language; it must not make sibling sessions
unavailable.

All remaining ADR-0011 lifecycle guarantees continue to apply: sessions stay keyed by
`LspServerId`, extension routing isolates document traffic, `LspState` owns wire versions, document
text comes from live buffers, server edits use `EditSource::Lsp` through the transaction spine, and
every inbound request receives an answer.

### 3. `TaskService::catalog` receives the filesystem and remains synchronous

The trait becomes
`catalog(&self, root: &WorkspaceRoot, fs: &dyn FileSystemService) -> Vec<TaskSpec>`. Adapters read
configuration files through this service boundary; they never call `std::fs` and never invoke a
build tool to enumerate tasks. Node discovery reads `package.json`, so opening `F5` remains
immediate and deterministic, while the filesystem-less headless workspace entry point may keep an
empty catalog.

Asynchronous discovery through a worker is reconsidered in Phase 09. Gradle enumeration and npm
workspaces need it, and Java is the first phase whose required task model justifies the added
lifecycle and transport. Phase 08 does not pre-empt that decision.

### 4. Workspaces may add declared tasks to detected tasks

`.termesh/workspace.toml` may contain `[tasks.<id>]` tables with a label, program, and argument
array. These tasks are appended to adapter-discovered tasks rather than replacing them, and their
identities are namespaced so they cannot collide with adapter task ids. Programs and arguments
remain structured argv values rather than shell strings.

This escape hatch ships with the conventional Python `pytest` task. A Python project that follows
another convention therefore has a remedy in the same release that introduces the guess, without
losing other detected tasks in a polyglot workspace.

### 5. One text problem matcher serves every non-Cargo task

`npm run build` may invoke any compiler, linter, or test runner, so a decoder selected per adapter
would pretend to know more than the repository declares. One line-buffered matcher handles the
three Phase 08 shapes:

- `file(line,column): error CODE: message`;
- `file:line:column: message`;
- Python traceback frames of the form `File "path", line N`.

The matcher resolves safe relative paths against the task working directory, keeps task and
problem collections bounded, and leaves every output byte available to the display. Unrecognised
output produces no problem row but is otherwise untouched, so an unsupported format cannot mangle
the real task output. Cargo retains its existing JSON decoder.

## Consequences

**Polyglot state becomes explicit without breaking single-kind callers.** Existing uses of
`WorkspaceRoot::kind` keep a deterministic primary, while detection, task adapters, snapshots, and
agent context can use the complete ordered set. Root-only detection deliberately leaves monorepo
sub-project routing for later.

**Workspace open becomes cheaper and first use becomes more informative.** Unused language
servers consume no process or indexing resources. The model must retain configured recipes and
handle `Idle`, starting, indexing, ready, and per-language unavailable states honestly. A bad
recipe is discovered later, so its first-use error must be immediate, actionable, and isolated.

**Task discovery gains filesystem access but not process access.** The service signature changes
and callers must provide `FileSystemService`, preserving the OS boundary and pure rendering. Config
reads stay synchronous because they are bounded root files; build-tool enumeration remains a
future worker concern.

**Conventions have an escape hatch.** npm scripts reflect the manifest, lockfiles choose the
package manager, Python receives a small conventional default, and workspace-declared tasks extend
the catalog without removing detected tasks. Malformed configuration may omit that adapter's
tasks, but it must not discard tasks from other detected languages.

**Problem navigation broadens conservatively.** Common TypeScript, gcc-style, and Python lines
become navigable without a regex dependency or per-tool decoder graph. Unknown and multi-line
formats remain readable but do not become problems.

**The Phase 07 boundaries remain fixed.** `crates/lsp` still depends on exactly `termesh-core` and
`serde_json`; protocol types remain isolated; session identity, document routing, live-buffer sync,
wire version ownership, inbound-request responses, and transaction-safe server edits are
unchanged. No test requires Node, npm, Python, or a language server on `PATH`.

**The reordered roadmap is now justified.** TypeScript/JavaScript and Python validate the fresh
polyglot seam in Phase 08; Java bears its distinct launch and build-system cost in Phase 09; the
resulting multi-server product is what Phase 10 hardens for public beta. Go, Dart, nested projects,
and asynchronous task enumeration remain intentionally deferred.
