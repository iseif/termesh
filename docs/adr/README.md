# Architecture Decision Records

Every load-bearing decision in this project is recorded here, in [Nygard format][nygard]: the
context that forced a choice, the options considered, the one taken, and what it costs. They are
written to be read by someone who wasn't there.

If you are trying to understand *why* the code is shaped the way it is — why the agent can't call
arbitrary actions, why buffer edits go through a transaction instead of touching the rope, why
there's no async runtime — the answer is in one of these, and the code comments will usually tell
you which.

Changes to the **action registry**, the **transaction spine**, or the **ACP client** need a new ADR
before implementation. See [CONTRIBUTING.md](../../CONTRIBUTING.md).

| # | Decision | Status |
|---|---|---|
| [0001](0001-record-architecture-decisions.md) | Record architecture decisions | Accepted |
| [0002](0002-language-and-tui-framework.md) | Language and TUI framework: Rust + Ratatui | Accepted |
| [0003](0003-agent-integration-strategy.md) | Agent integration: ACP-first, tiered, with a free terminal-CLI fallback | Accepted |
| [0004](0004-placeholder-codename.md) | Placeholder codename | Resolved at `0.1.0` — the name is `termesh` |
| [0005](0005-workspace-filesystem-service.md) | Workspace filesystem service: concurrency, watching, and ignore semantics | Accepted |
| [0006](0006-transaction-spine.md) | The transaction spine: `ChangeSet` representation, rebasing, and undo | Accepted |
| [0007](0007-acp-client.md) | The ACP client: dependency, transport, context, and the review loop | Accepted |
| [0008](0008-terminal-service-and-acp-execution.md) | One terminal service for human shells and ACP command execution | Accepted |
| [0009](0009-search-task-execution-and-acp-semantics.md) | Search and task execution use workers, managed terminals, and stable ACP | Accepted |
| [0010](0010-git-service-actions-and-agent-semantics.md) | Git uses a CLI-backed worker, explicit index commits, and stable ACP command approval | Accepted |
| [0011](0011-language-servers.md) | Language servers use long-lived sessions and transaction-safe edits | Accepted |
| [0012](0012-polyglot-workspaces.md) | Polyglot workspaces detect every root language and start sessions lazily | Accepted |
| [0013](0013-java-language-support.md) | Java support uses wrapper-owned JDT LS launch and conventional build tasks | Accepted |
| [0014](0014-beta-hardening.md) | Beta hardening: layered config, offered-not-applied crash recovery, and a published ACP session-restore boundary | Accepted |
| [0015](0015-acp-session-modes.md) | ACP session modes are surfaced and changed explicitly, never escalated for you | Accepted |
| [0016](0016-edit-permission-review.md) | An edit the agent asks permission for is reviewed as a diff, not approved blind | Accepted |

## Reading the older ones

An accepted ADR is a record of what was decided and when, so these are not edited to match later
reality — where an ADR was overtaken by events, a later ADR says so and the older one keeps its
original text. ADR-0004 is the clearest example: it decided to ship under a placeholder codename,
that codename turned out to be taken, and rather than rewriting the decision it gained a
*Resolution* section recording the outcome.

Three artifacts of the pre-`0.1.0` build are named in these ADRs and need a word of explanation:

- **`CLAUDE.md`** was the internal build brief — a standing set of invariants and a
  phase-by-phase working protocol. It is not part of this repository. Its invariants, which are the
  part that constrains contributions, now live in [CONTRIBUTING.md](../../CONTRIBUTING.md).
- **"Phase NN"** refers to the staged build plan in [ARCHITECTURE.md §16](../../ARCHITECTURE.md).
  Phases 00–10 are complete as of `0.1.0`; §16 still lists them, and Phase 11 onward is the
  post-beta roadmap.
- **`docs/release-checklist-0.1.0.md`**, named in ADR-0014, was a one-off operational runbook for
  cutting `0.1.0` — reserving names, provisioning signing certificates, publishing, tagging. It is
  owner-run and version-specific, so it lives with the project's private history rather than here.
  What matters for reading ADR-0014 is only that such steps were deliberately kept out of the
  automated build, which is the decision it records.

[nygard]: https://cognitect.com/blog/2011/11/15/documenting-architecture-decisions
