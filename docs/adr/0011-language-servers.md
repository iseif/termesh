# 11. Language servers use long-lived sessions and transaction-safe edits

Date: 2026-08-11

## Status
Accepted

## Context

Phase 07 makes Termesh a language-server client for one flagship language, Rust through
rust-analyzer. The server must stay synchronized with live buffers, publish diagnostics and
progress without being asked, answer navigation and language-intelligence requests, and return
edits that re-enter the transaction spine. Diagnostics and symbols also become bounded agent
context drawn from the same model-owned state the human sees. Phase 07 adds agent context, not
agent tools: it does not expose language actions through ACP or add any other agent tool surface.

ARCHITECTURE.md contradicts itself about the phase's product scope. Section 14's trimmed MVP lists
only diagnostics, go-to-definition, hover, and completion, while Section 16 requires the full
Phase 07 set: diagnostics, hover, completion, definition, references, document and workspace
symbols, rename, code actions, and formatting. The project owner resolved this contradiction in
favour of the full Section 16 list. Section 14 will be corrected in the phase's closing
documentation commit; this ADR records the governing resolution now.

Accepted decisions constrain the design:

- ADR-0005 makes synchronous service traits on worker threads the default OS boundary and names
  LSP as a later service expected to copy that shape unless a later ADR records a deviation.
- ADR-0006 requires every mutation, including server-authored edits, to enter through an
  `EditTransaction` and preserves version checks and undo at the buffer boundary.
- ADR-0007 keeps ACP wire details inside `agent`; language-server protocol details likewise stay
  behind `LanguageService` and do not leak into widgets, the view, or agent context assembly.
- ADR-0009 finds that stable ACP has no portable client-owned custom-tool registration mechanism;
  Phase 07 therefore adds language context for the agent, not language tools.
- ADR-0010 supplies the document structure and demonstrates that user-invocable feature behavior
  belongs on the stable action registry.

LSP is not a request-in/reply-out protocol. A server emits notifications such as
`textDocument/publishDiagnostics` and `$/progress` unsolicited, and it sends client-directed
requests such as `workspace/configuration` that must receive answers. Its document versions also
have stricter monotonicity requirements than the editor's buffer staleness token. Those properties
make the service transport, document-sync rules, action names, and server-edit path load-bearing
architecture rather than implementation details.

## Options considered

### A. A typed language service over a long-lived stdio session with hand-rolled wire types

Keep protocol-neutral requests and events in `core`; put framing, JSON translation, and process
supervision in `crates/lsp`; and have the model own all language state. Standard threads and
channels provide a continuously polled session, while a small hand-rolled wire layer covers only
the methods Phase 07 uses. The complete allowed dependency set for `crates/lsp` is `termesh-core`
and `serde_json`; no other internal or external dependency is permitted. This keeps protocol field
names at the wire boundary.

### B. Adopt `lsp-types` and possibly `tower-lsp`

Upstream types reduce handwritten serialization and track a broader specification surface, but
Phase 07 needs only a bounded subset. `lsp-types` adds a large, fast-moving public type graph, and
`tower-lsp` also brings tokio. Neither cost is justified for the flagship-language slice, and the
`LanguageService` boundary leaves the wire implementation replaceable if later evidence changes
that trade-off.

### C. Copy the ADR-0005 request/response worker exactly

This would preserve the established synchronous trait and one-request/one-reply worker transport.
It cannot represent unsolicited notifications or server-to-client requests without special-case
channels and hidden control flow, and a server can hang if those inbound requests go unanswered.
The worker's architectural shape remains useful, but its transport contract is insufficient.

We take Option A.

## Decision

### 1. `LanguageService` is a long-lived session, not a request/response worker

Phase 07 takes the Git service's shape with the agent service's transport: a typed service boundary,
protocol-neutral messages, model-owned state, and test doubles, paired with a continuously running
bidirectional session that can emit events and answer inbound requests independently of an
outstanding application request. This is a knowing deviation from ADR-0005's request/response
worker transport, required by the LSP protocol rather than a new default for other services.

Sessions are keyed by `LspServerId`, the application message carries that server identity, and
documents are routed to a session by extension from the first implementation commit. Phase 07
starts exactly one rust-analyzer session, but preserving the identity and routing seam now makes a
later polyglot workspace additive instead of requiring every outbox and stale-response correlation
guard to be refactored.

### 2. Wire types are hand-rolled over `serde_json`

LSP wire types live in `crates/lsp/src/protocol.rs` and cover the bounded method set the product
uses. `crates/lsp` may depend only on `termesh-core` and `serde_json`; that pair is the complete
allowed dependency set. It adds neither `lsp-types` nor `tower-lsp`, does not use tokio, and may not
opt into another existing workspace dependency. This choice stands on its own merits: it keeps the
dependency surface small, confines protocol names to one boundary, and remains replaceable behind
`LanguageService`.

A server recipe may carry `initializationOptions` as raw JSON. Servers we expect to add later,
including Eclipse JDT LS and pyright, do not start usefully without server-specific initialization
data. Keeping that value in the recipe lets those servers remain recipe additions rather than
changes to the protocol-neutral model.

The current agent implementation is not precedent for this choice. ADR-0007 Section 1 decided to
depend on `agent-client-protocol-schema`, but that crate appears in neither
`crates/agent/Cargo.toml` nor `Cargo.lock`; `crates/agent` hand-rolls its wire messages and
translation instead. The root `Cargo.toml` comment attributing the workspace's Rust 1.88 floor to
that absent schema dependency is therefore also inaccurate. Those mismatches are pre-existing
architecture and MSRV documentation debt to resolve separately, not precedent that authorizes the
LSP design.

### 3. Framing uses the LSP base protocol

The stdio codec uses LSP base-protocol framing: `Content-Length` headers, a blank line, and an
exactly sized JSON body. It handles split headers and bodies without treating byte chunks as
message boundaries.

Every inbound JSON-RPC request receives a response. Supported client requests receive an
appropriate valid response; an unknown or unsupported method receives a JSON-RPC error with code
`-32601` (`Method not found`). No server request is left unanswered to hang the session.

`crates/lsp` defines its own JSON-RPC `Message` enum rather than sharing the agent enum. ACP uses
newline-delimited JSON while LSP uses header framing, and a shared wire type in `core` would violate
`core`'s zero-dependency boundary by pulling in `serde_json`. The small duplication keeps both
protocols isolated and explicit.

### 4. `LspState` owns wire document versions

The language layer maintains a strictly increasing version for each open document and sends that
number on the wire. It never forwards `Buffer::version`: the buffer number is a transaction-spine
staleness token, and reload-in-place can reset it while the protocol requires monotonic document
versions. The same `ChangeSet` stream drives both systems, but their counters serve different
contracts.

### 5. `didOpen` and `didChange` send buffer text

Document synchronization reads text from the live `Buffer`, never from disk. Unsaved edits exist
only in the buffer, and the editor normalizes file content in ways that can make disk byte offsets
differ from buffer offsets. A disk read would therefore publish stale text and invalidate later
positions. Reload-in-place is represented as close-then-open rather than reconstructing content
from the filesystem behind the buffer.

### 6. `didChange` sends one incremental replaced span

`ChangeSet::changed_span` computes the minimal before/after span enclosing a transaction, and the
translator emits that span as exactly one incremental content change. This preserves transaction
stream semantics across typing, undo, and redo while avoiding full-text synchronization on the
editing hot path. Multi-range incremental changes are a later optimization, not part of the Phase
07 contract.

### 7. `WorkspaceEdit` applies asynchronously through buffers

Every server-authored edit enters through a live buffer as an `EditTransaction` with
`EditSource::Lsp`; no language path writes a rope or file directly. Closed target files are opened
through the asynchronous filesystem service, so a multi-file edit is held pending until every
target buffer is available. A read failure abandons the edit with an actionable message.

When the protocol supplies a document version, an edit whose version is stale relative to the
version sent to the server is refused rather than applied blind. Edits for each file are composed
into one transaction, giving one undo group per file. Multi-file undo is deliberately per-file,
not atomic across the workspace.

### 8. Language actions keep stable ids and share a title prefix

The existing `editor.goto_definition` action keeps its published id and F12 binding. New actions
use the `lsp.*` namespace: `lsp.hover`, `lsp.completion`, `lsp.references`,
`lsp.symbols.document`, `lsp.symbols.workspace`, `lsp.rename`, `lsp.code_action`, `lsp.format`,
and `lsp.restart`.

Go-to-definition and every new language action use a `Code: ` title prefix in the flat palette.
Only the existing action's title changes; its id does not. Widgets and overlays do not gain private
mutation paths: user-invocable language behavior continues to resolve through the registry.

## Consequences

**A deliberately different transport.** `LanguageService` preserves the service boundary and
single-owner message flow established by earlier phases, but a long-lived bidirectional session
replaces ADR-0005's one-request/one-reply worker contract for LSP only.

**A small protocol surface that Termesh owns.** Framing and roughly the Phase 07 method set require
careful translator tests, including malformed messages and server-to-client requests. The cost is
accepted in exchange for no new dependency, no tokio runtime, and a wire layer confined to
`crates/lsp`.

**Correct live-document semantics.** Language versions remain monotonic independently of buffer
reloads, text always reflects unsaved editor state, and one replaced span carries each transaction.
Undo and redo must append to the same document-change outbox as ordinary edits.

**Server edits preserve the transaction spine.** Rename, code actions, completion edits, and
formatting remain asynchronous, version-checked, undoable buffer operations. A multi-file edit can
partially fail to open but can never silently write stale data to disk.

**The polyglot seam exists before multiple languages do.** Phase 07 still ships exactly one Rust
recipe and one running server. Session identity, extension routing, and raw recipe initialization
options make later language recipes additive without broadening this phase's product scope.

**Stable action names remain compatible.** Existing consumers of `editor.goto_definition` do not
break, while the shared `Code: ` prefix gives the language family a coherent palette surface.

**Pre-existing agent/MSRV divergence remains explicit debt.** This ADR neither ratifies nor fixes
the mismatch between ADR-0007, the agent implementation, the lockfile, and the root MSRV comment.
That cleanup is outside Phase 07 Task 0 and requires its own reviewed change.

**Approval was a hard gate.** No production implementation began while this ADR was `Proposed`.
The project owner approved it before implementation; Phase 07 then satisfied the full gate, and
the closing documentation commit accepted this ADR and corrected ARCHITECTURE.md Section 14.
