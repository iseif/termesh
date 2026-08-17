# 15. ACP session modes are surfaced and changed explicitly, never escalated for you

Status: Accepted

## Context

The product's central claim is that an agent's edits arrive as reviewable diffs. Recording the ACP
session against real agents, rather than trusting the scripted double, showed it does not engage —
for two separate reasons, neither visible from the code, and only one of which this ADR fixes.

**No agent tested calls `fs/write_text_file`, though the client advertises it.** opencode (native
ACP), Codex (through `@zed-industries/codex-acp`), and JetBrains Junie (`junie --acp`) each edit
the file on disk themselves and send the ACP `diff` content block only so the client can display
it. Captured from the wire in every case: zero write requests in either direction, and the file
already changed. By the time the human accepts, the watcher has reloaded the buffer and the hunks
are `Satisfied` rather than `Clean`. Review becomes a notification of something already done. That
is a property of each agent and no mode changes it — it is recorded here because it is the reason
this decision cannot be justified by "it makes diff review work."

**Codex is additionally blocked from acting at all.** It opens its session in a `read-only` mode
and offers `auto` and `full-access` alongside it. The mode is reported in the `session/new` result,
which this client parses for a session id and nothing else. There is no way to leave read-only, so
Codex is permanently unable to edit — it will ask for approval that never arrives. *That* is what
session modes fix, and it is worth fixing on its own: an agent that cannot act is not an agent.

Session modes are a stable part of the protocol:

- the `session/new` result carries `modes: { currentModeId, availableModes: [{ id, name,
  description? }] }`
- the client changes it with `session/set_mode { sessionId, modeId }`
- the agent reports changes with a `session/update` notification whose `sessionUpdate` is
  `current_mode_update`, carrying `modeId`

Nothing about them is optional-but-experimental, and an agent that offers modes and is never given
one is an agent the user cannot use.

## Options considered

**A. Set a permissive mode automatically when the session opens.** One line, unblocks Codex, and
the diff review starts working immediately. It also silently grants an agent authority to edit
files because a mode list happened to contain something called `auto` — on a session the human
opened to ask a question. The names are the agent's, not ours: `full-access` on Codex means editing
outside the workspace and reaching the internet without asking. Choosing among them on the user's
behalf, by pattern-matching strings, is exactly the kind of quiet escalation the permission model
exists to prevent.

**B. Ignore modes and document the limitation.** Honest and cheap, and wrong: it makes a whole
class of agents unusable for the feature the project is built around, and the fix is small.

**C. Surface the mode, and make changing it one explicit action.** The pane shows which mode the
session is in. A registry action lists what the agent offers and switches on request. The agent's
default is never overridden. Chosen.

## Decision

### 1. The agent's default mode stands

termesh never sends `session/set_mode` unprompted — not at session start, not in response to a
refused edit, not because a mode has a permissive-sounding name. Whatever the agent chose is what
the session runs in until a human changes it. An agent that defaults to read-only is an agent being
careful, and overriding that from the client would defeat the reason it did.

### 2. The current mode is visible

The Agent pane shows the mode name alongside the agent. Without it, a read-only Codex session looks
identical to a broken one: the agent explains what it would change and nothing happens. The mode is
the answer to "why did nothing happen", so it belongs on screen rather than in a log.

### 3. Changing it is an action, not a keystroke or a prompt

`agent.mode` joins the registry and the palette like everything else (CONTRIBUTING: one command
surface). It lists `availableModes` with their descriptions — the agent's own words, which say what
the mode permits — and sends `session/set_mode` for the chosen one. Permission-gated, because it
changes what the agent is allowed to do.

### 4. Modes are optional, and their absence is not an error

An agent that reports no modes gets no mode UI, no requests, and no behaviour change. opencode is
unaffected by all of this. A `session/set_mode` for an agent that never offered modes is never
sent.

### 5. The agent's report is the truth

The current mode is whatever the agent last said it is, and it has three ways of saying it: the
`session/new` result, a successful reply to `session/set_mode`, or a `current_mode_update`
notification. The last one wins whenever it arrives, because an agent may change mode on its own.

The rule this enforces is that the client never renders a mode it has not been told about. A
success reply *is* being told: the agent has accepted the change and said so. An error reply is
the refusal case, and it leaves the mode exactly where it was.

Requiring the notification specifically would be stricter than the protocol and wrong in
practice. The spec does not say what a `session/set_mode` response contains or whether a
notification follows a client-initiated change, and `@zed-industries/codex-acp` answers with a
bare `{}` and never notifies —
so a client waiting for the notification would leave the pane reading `read-only` forever, for
the one agent this decision exists to unblock. Verified on the wire before writing it down.

### 6. The mode is not remembered across sessions

Nothing persists a chosen mode. Restoring "full access" into a fresh session because it was chosen
once, for some other task, is escalation with extra steps. Sessions do not survive restart anyway
(ADR-0014 §4), so there is no continuity being broken.

## Consequences

Codex becomes usable: a human can move the session to `auto` and Codex will act instead of
declining every edit. That is the block this removes, and it is worth removing — an agent that
cannot do anything is not an agent.

It does **not** make the diff-review loop engage for Codex. That claim was in an earlier draft of
this section and the wire disproved it: driven through `session/set_mode` into `auto` and asked
for an edit, Codex made zero `fs/write_text_file` calls and changed the file on disk itself. Junie
does the same, as does opencode. All three send an ACP `diff` content block, which is a display
payload rather than a request to write, and is why the change looks reviewed when it is not. So of
the three agents tested, **none** routes writes through the client, and modes change that for none
of them.

Reviewable diffs therefore depend on a property of the agent that this client can advertise
(`fs/writeTextFile`) but cannot compel. The review path is right and tested; what is missing is an
agent that uses it. The support boundaries name each agent and what it did on the wire, rather
than listing agent names under a promise the client cannot keep alone — and the README and landing
page say the same thing, because that sentence is the one a reader tests first.

A mode is a permission boundary the human sets and the agent reports. Making it visible costs a
line of the pane; making it changeable costs an action. Neither lets the client widen what an agent
may do without being asked, which is the property worth keeping.
