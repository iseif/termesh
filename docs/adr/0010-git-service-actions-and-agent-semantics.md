# 10. Git uses a CLI-backed worker, explicit index commits, and stable ACP command approval

Date: 2026-08-10

## Status
Accepted

## Context

Phase 06 must deliver a status model, branch selector, diff viewer, commit, fetch/pull/push,
conflict indicators, palette actions, agent Git context, and human approval for agent-proposed
commits. Its exit criterion is `edit -> review -> commit -> push`.

Accepted decisions constrain the design:

- ADR-0005 makes synchronous service traits on worker threads the default OS boundary and names
  `GitService` as a service that must follow that pattern unless a later ADR supersedes it.
- ADR-0007 requires Git diff to join the small per-turn workspace context when available and keeps
  ACP wire types isolated in `agent`.
- ADR-0008 makes standard ACP `terminal/create` plus exact human approval the supported command
  execution path.
- ADR-0009 records that stable ACP has no portable client-owned custom-tool registration mechanism
  and rejects both a premature MCP sidecar and unstable MCP-over-ACP.
- ARCHITECTURE.md §14 explicitly defers hunk staging while requiring status, changed files, diff,
  commit, fetch/pull/push, and branch checkout.
- CLAUDE.md requires an ADR before changing the action registry or ACP semantics.

The `git` crate currently contains only an empty `GitService` placeholder. The action registry has
placeholder `git.stage` and `git.commit` entries but no status surface, unstage, branch, remote
actions, worker, model state, fake, or UI. The application sends no Git context to the agent.

The central product question is the commit boundary. Automatically staging tracked changes would
make “Commit” convenient but would contradict the status/review surface: a file shown as unstaged
could enter the commit without an explicit decision. The approved Phase 06 behavior is that commits
contain only explicitly staged files.

## Options considered

### A. A typed Git service backed by structured Git CLI commands

Run Git only behind `GitService` on a worker thread. Parse stable machine-readable output into
protocol-neutral state. Human actions use the service; standard ACP Git commands continue through
the existing permissioned PTY path. This follows ADR-0005, respects the user's Git configuration,
and adds no heavy dependency.

### B. Run every Git operation in the managed terminal

This reuses Phase-04 execution but requires durable application state to be reconstructed from
terminal-oriented output. Passive refreshes would create processes whose output surface is a PTY,
and a widget could not request status without coupling to terminal lifecycle. Rejected because Git
is an application service with typed state, not only a visible command.

### C. Use `git2`/libgit2

This provides structured APIs but adds a heavy native dependency and authentication/configuration
behavior that can differ from the user's Git CLI. Rejected for the trimmed MVP. `GitService` keeps
this replaceable if future evidence justifies it.

We take Option A.

## Decision

### 1. `GitService` is synchronous and worker-owned

`GitService` follows ADR-0005: synchronous, object-safe, and called only by `GitWorker`. Widgets,
rendering, and the model never invoke Git or the filesystem directly. The worker emits typed
`GitEvent`s onto the existing `AppMessage` channel.

Protocol-neutral request, event, status, branch, and diff types live in `core`; CLI parsing and
process details live in `git`. `test-support` provides `FakeGitService` using the same worker and
event path as production.

The production backend invokes the external Git CLI with `std::process::Command`, structured argv,
an explicit cwd, and no shell. Paths are individual argv elements after `--`. No heavy dependency
is introduced.

### 2. Status is porcelain-v2 state owned by the model

The backend reads `git status --porcelain=v2 --branch -z` and produces:

- repository root;
- head branch or detached/unborn state;
- upstream and ahead/behind counts;
- per-path index and worktree state;
- rename origins;
- untracked paths;
- explicit conflict state.

The model owns the latest good snapshot plus loading, stale, and error state. Status refreshes on
workspace open, relevant filesystem activity, native Git operation completion, managed terminal
exit, and explicit user refresh. `git.show` carries that explicit refresh: it re-reads status on
every invocation, and re-invoking it while the status surface is open refreshes that surface in
place rather than opening a second one. Repeated triggers coalesce, and typed request IDs make late
results from an older workspace or selection harmless.

A refreshed status surface re-anchors its selection on the `(group, path)` it was pointing at, not
on its row index: a background refresh can insert rows above the cursor, and an index-only clamp
would move the selection onto a file the developer never chose — which the next stage or unstage
would then act on.

Passive reads (`status`, `diff`) pass `--no-optional-locks`, because a refresh fires on every
coalesced filesystem batch and must not compete for `.git/index.lock` with the developer's own Git
commands in the managed terminal.

Diff requests are explicitly `Worktree` or `Index`, use bounded no-color/no-external-helper unified
output, and mark truncation. A file may expose both targets.

### 3. The index is the exact commit boundary

Phase 06 stages and unstages whole files only. Hunk staging remains deferred by ARCHITECTURE.md
§14.

`git.commit` commits the current index and never runs an implicit add. It refuses when the index is
empty or the message is blank. Unstaged worktree changes remain untouched before and after the
commit. There is no “commit all” action in Phase 06.

This makes the review surface honest: every path in a commit first appeared in the Staged group as
the result of an explicit stage action.

### 4. Remote and branch mutations are conservative

Phase 06 supports existing local branch checkout, fetch, pull, and push.

- Checkout selects an existing local branch and lets Git reject unsafe dirty-worktree transitions.
- Pull is `--ff-only`; Termesh does not create an implicit merge commit or rebase.
- Push never uses force options. With an existing upstream it runs plain `git push`; without one,
  it publishes the current local branch with `git push --set-upstream origin <branch>`. `origin` is
  the deliberate first-push default, and a missing `origin` is reported rather than guessed around.
- Remote operations disable interactive terminal prompting. Configured credential helpers and SSH
  agents may authenticate; otherwise the bounded error directs the developer to the managed
  terminal.

Branch creation/deletion/rename, merge/rebase UI, history rewriting, and force push are deferred.
Conflicts created outside this surface remain first-class status indicators and are resolved by
ordinary editor changes followed by explicit staging.

### 5. Git behavior stays on the action registry

The registry contains:

```text
git.show
git.stage
git.unstage
git.commit
git.branch.checkout
git.fetch
git.pull
git.push
```

The existing `Ctrl+G` binding remains attached to `git.stage`; it acts on the selected Git status
row and otherwise opens the status surface with an instruction to select a path. Other actions are
palette-first during Phase 06. The detailed status, diff, branch, and commit interfaces use the
application's existing overlay/prompt pattern; explorer decorations and the status bar remain the
persistent Git indicators.

No widget owns a private mutation path. Overlay keys resolve to these actions or overlay-navigation
commands.

### 6. Agent context and proposed commits use stable ACP

Every agent turn receives a bounded rendering of the same model-owned branch, status, and staged
plus worktree diff shown to the human. Truncation is explicit. Prompt assembly performs no Git I/O.

Stable ACP still cannot register a portable client-owned `git.commit` tool (ADR-0009). We do not
add an MCP sidecar, depend on unstable MCP-over-ACP, or claim a custom wire method exists.

An agent proposes a commit by requesting standard ACP `terminal/create` with structured argv such
as `git commit -m <message>`. ADR-0008's permission flow displays the exact program, every argv
element, cwd, and environment before anything starts. Approval runs the command through
`PtyService`; rejection starts no process. The terminal remains visible and queryable to both the
human and agent. Its completion triggers a native status refresh.

The same rule applies to agent-proposed fetch, pull, push, checkout, or arbitrary Git commands.
Phase 06 creates no special standing grant and never reinterprets a shell string as a safe Git
action. Thus “agent can propose commits (human approves)” is satisfied entirely through stable ACP
and the already accepted exact-command permission boundary.

### 7. Failure is local, bounded state

A missing Git executable, non-repository workspace, malformed output, hook failure, authentication
failure, checkout refusal, non-fast-forward pull/push, or conflict never exits the IDE. The last
good snapshot may remain visible with a stale/error marker. Output is bounded and shown to the
developer.

The real service never logs or renders unbounded command output. Paths remain `PathBuf` until the
view uses lossy display, preserving non-UTF-8 identity.

## Consequences

**One typed human path.** Native human Git actions and passive refreshes share `GitService`; there
is no UI-side process execution or filesystem access.

**One standard agent execution path.** Agent Git commands share the existing ACP permission and
managed PTY behavior. There is no ACP-specific Git runner and no unstable tool transport.

**Explicit staging is non-negotiable.** Commit behavior may feel less magical than auto-stage, but
the displayed index is always the actual commit boundary and unstaged edits cannot enter silently.

**No heavy dependency.** The workspace's existing Git CLI requirement becomes active. Users retain
their normal configuration and credential helpers. Machines without Git retain every non-Git IDE
feature.

**The action surface grows deliberately.** Status, stage/unstage, commit, branch, and remote
operations are discoverable through the palette. A future persistent tool-window framework can
host the same actions and model without changing service semantics.

**Fakes and headless proof are mandatory.** `FakeGitService` covers model and render behavior
without a repository. `--dump-frame --git-demo` renders deterministic status and diff state.
Parser tests cover porcelain-v2 edge cases; a temporary-repository integration test proves that a
commit includes staged content and preserves unstaged worktree changes.

**Documentation follows implementation.** When the phase matches this ADR and passes the full
gate, update README/CLAUDE roadmap state, glossary, user workflow documentation, and this ADR's
status to `Accepted`.

**Deferred deliberately.** Hunk staging, history/log UI, commit amend/signing UI, stash, merge,
rebase, cherry-pick, reset, reflog, submodules, force push, hosting-provider workflows, and custom
ACP/MCP Git tools remain outside Phase 06.
