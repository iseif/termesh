# 16. An edit the agent asks permission for is reviewed as a diff, not approved blind

Status: Accepted

## Context

ADR-0015 recorded that no ACP agent tested routes its writes through this client: each edits the
file itself and sends the ACP `diff` content block only so it can be displayed. That was captured
from the wire and is still true. The conclusion drawn from it — that reviewable diffs cannot engage
with any available agent — was too broad, because it tested the wrong thing.

The client's review gate does not have to be `fs/write_text_file`. It has to be *something the
agent waits on before writing*. Recording Codex in `read-only` mode shows exactly such a point:

```json
"toolCall": { "toolCallId": "call_O0Jd…", "kind": "edit", "status": "pending",
              "title": "Edit …/src/pricing.rs",
              "content": [{ "type": "diff", "path": "…/src/pricing.rs",
                            "oldText": "\npub fn vat(cart: &Cart) -> u32 {\n",
                            "newText": "\n/// Total incl. VAT.\npub fn vat…" }] }
```

That arrives as `session/request_permission`, before the file is touched, and carries the whole
proposed change. Answering it decides what happens:

| answer | outcome | file on disk |
|---|---|---|
| denied (`cancelled`) | turn ends `cancelled` | **unchanged** |
| approved | agent performs the edit itself, zero `fs/write_text_file` | changed by the agent |

So the gate is real and it is pre-write. What the agent keeps is the *writing*; what the client
gains is the *deciding*, which is the half the product claim is about.

What this client does with that today: `crates/agent/src/protocol.rs` reads `toolCall.title` and
`toolCall.rawInput.command` and nothing else. `toolCall.content[]` is discarded. A Codex edit
request therefore renders as a single line — `Edit /long/path/pricing.rs` — with an allow/deny
prompt. The human is asked to approve an edit they cannot see. That is the worst version of a
permission prompt: it trains the answer "yes" because refusing costs more than reading.

### What the other agents do

Measured the same way, with the same prompt and the same denial:

| Agent | asks permission to edit | reject option offered | edit gated |
|---|---|---|---|
| Codex (`read-only`) | **yes**, with a `diff` block | `abort` — ends the whole turn | **yes** — denial held, file unchanged |
| Codex (`auto`) | no | — | no — writes directly |
| opencode, `permission.edit: "ask"` | **yes**, with a `diff` block | `reject_once` — declines just this edit | **yes** — denial held, file unchanged |
| opencode, as shipped | no | — | no — writes directly |
| JetBrains Junie | no — edits without asking | (gates `cargo check` only, `yes`/`no`) | no |

Two agents stop and ask, and both honour a refusal. Junie is the honest counterexample: it gates
commands and not edits, so this decision buys nothing there.

opencode is the interesting case, because it is gated only when told to be. Out of the box it
edits without asking; adding

```json
{ "permission": { "edit": "ask" } }
```

to `opencode.json` makes it request permission for every edit, with the standard ACP option kinds
`allow_once` / `allow_always` / `reject_once`. That is a better reject than Codex offers, and it
means the answer to "can this agent be reviewed" is a configuration question rather than a fixed
property of the agent — which the support boundaries have to state as such, since a user who never
sets that key will conclude the feature does not work.

## Options considered

**A. Leave it. Permission prompts stay one-liners.** Cheap, and it keeps the review path pointed at
`fs/write_text_file` where it is well tested. It also means the one agent that *does* stop and ask
before editing gets an approval prompt with the diff deleted from it, which is worse than having no
gate at all — an unreadable prompt is a prompt that gets approved.

**B. Apply the diff ourselves on approval.** Parse the `diff`, approve the permission, and apply the
change as an `EditTransaction` so it flows through the spine like any other proposal. This is
wrong: the agent also applies it. The file would be written twice, and the second writer would be
racing our own watcher. The `diff` block describes an edit the agent is about to make, not one it
is delegating.

**C. Review the diff, then answer the permission with the human's decision.** The proposal is shown
before anything is written, using the review UI that already exists; accept approves the request and
the agent proceeds; reject denies it and nothing happens. Chosen.

## Decision

### 1. A permission request carrying a `diff` is a proposal, and is shown as one

When `session/request_permission` has a `toolCall` whose `content[]` includes a `diff` entry, the
client renders it through the existing proposal path — the same diff rendering, the same gutter
marks — rather than as a one-line tool summary.

A permission request without a `diff` is unchanged: it stays the one-line prompt it is today.

### 1a. A permission diff may be a whole file or a fragment, and the client must tell which

This is the part that cannot be reused as-is, and getting it wrong destroys a file.

`crates/agent/src/proposal.rs` opens by stating the assumption the whole anchoring design rests
on: *"ACP does not hand us range edits. It hands us whole-file before-and-after text."* That is
true of some agents and false of others, and the same field carries both. Asked for the identical
one-line change to the identical 365-byte file, on the same `content[] type: "diff"`:

| Agent | `oldText` | `newText` | shape |
|---|---|---|---|
| opencode | 365 bytes, **byte-identical to the file on disk** | 386 bytes | whole file |
| Codex | 34 bytes, `"\npub fn vat(cart: &Cart) -> u32 {\n"` | 55 bytes | **fragment** |

`EditProposal::new` diffs `old_text` against the buffer to derive hunks. Given opencode's payload
that is correct. Given Codex's, it derives "replace all 365 bytes with these 55" — a
correct-looking proposal that silently deletes the rest of the file when accepted.

Nothing in the protocol distinguishes them. Both are `type: "diff"` with the same two string
fields; only comparing `oldText` against the buffer reveals which arrived, and a fragment that
happens to be long is indistinguishable from a small whole file.

So the client decides by inspection, not by trust: if `oldText` matches the buffer in full, the
existing whole-file derivation runs unchanged. Otherwise it is treated as a fragment and anchored
by locating `oldText` **within** the buffer, building the hunk at that offset. Three cases the
fragment path must answer rather than assume:

- **no match** — the buffer has moved under the agent. Report it as a conflict and let the human
  decide with the diff visible; never guess an offset.
- **more than one match** — a short fragment can appear repeatedly. Ambiguous is a conflict, not a
  coin toss; picking the first occurrence is how the wrong function gets edited.
- **exactly one match** — anchor there.

The two shapes therefore take different entry points behind one classifier, and the fragment path
carries this reasoning at its definition. What must not happen is a single derivation that assumes
whole-file and is fed a fragment, because that failure is silent, looks correct on screen, and
costs the file.

### 2. Accepting approves the request; the client does not write

The agent is going to make this edit itself. Accepting means selecting the permission's approving
option and nothing else — no `EditTransaction`, no write. The change arrives the way any external
change arrives: the watcher notices and the buffer reloads.

This is the opposite of the `fs/write_text_file` path, where the client is the writer, and the
difference must be visible in the code rather than smoothed over. Applying the diff *and* approving
would write the same edit twice.

### 3. Rejecting denies the request, and the file is untouched

Verified: a denied edit permission leaves the file byte-identical and ends the turn `cancelled`.
That is the product claim — *nothing reaches disk until you accept* — holding against a real agent
for the first time.

### 4. Per-hunk accept is not offered on this path, and says so

A permission answer is one decision for one tool call. There is no way to approve two hunks of an
edit and refuse a third. Rather than showing per-hunk keys that cannot work, the prompt on this
path offers accept and reject only, and the pane says why. The per-hunk path remains available for
agents that write through the client; it is not being removed.

### 5. Reject is as coarse as the agent makes it

Codex offers `approved` and `abort`, and `abort` ends the whole turn rather than declining one
edit. opencode offers `allow_once` / `allow_always` / `reject_once`, so rejecting there declines
that edit and the turn continues. Junie offers `yes`/`no`.

The client selects the agent's own least-permissive option and does not pretend the outcome is
finer than it is: where refusing costs the turn, the prompt says the turn will end. Inventing a
"reject just this one" that a given agent cannot deliver would be a lie told by the UI — and since
the same key does different things depending on the agent, the prompt has to read the offered
option kinds rather than assume.

### 6. This does not change what the support boundaries claim

The boundaries say no tested agent routes writes through the client. That stays true and stays
written down. What gets added is that an agent which *asks before editing* can be reviewed, that
Codex in `read-only` does, and that Junie does not — per agent, measured, with the same table shape
already used there.

One existing sentence there is now inverted and must be rewritten rather than appended to. The
session-mode entry currently says moving Codex to `auto` "buys you a working agent, not a
reviewable one", which reads as though `auto` is the better mode. It is the opposite: `auto` is
the mode in which Codex writes unreviewed, and `read-only` is the mode in which it asks first and
can be reviewed. The safer mode is the reviewable one, and the docs should say so plainly.

## Consequences

The central claim becomes demonstrable end to end against a real agent, and against two of them:
put Codex in `read-only` — or opencode behind `permission.edit: "ask"` — ask for a change, see the
diff, reject it and watch nothing happen, or accept it and watch it land. That is the recording
that has been missing, and it needs no scripted double. opencode is the better subject for it,
since `reject_once` leaves the turn alive and the demo can show a rejection followed by a second
attempt.

ADR-0015 turns out to be the precondition rather than a consolation. `Agent: Session Mode` is how a
human reaches the mode where edits require approval — and the mode that makes Codex *safer* is the
one that makes it *reviewable*, which is a better story than the one ADR-0015 set out to tell.

The cost is a second review path with different semantics from the first — client-as-writer versus
client-as-gate — and two paths that look identical on screen but differ in who writes are a real
source of future confusion. That is the reason for decisions 2 and 4 being explicit about which one
is running.

It buys nothing for Junie, and nothing for Codex in `auto` or opencode left at its defaults. An
agent that does not stop cannot be gated by a client, and no amount of protocol handling changes
that.

For opencode the difference is one config key, which makes the documentation load-bearing: a user
who never sets `permission.edit: "ask"` will try the feature, watch the file change without being
asked, and conclude it does not work. The support boundaries have to give the key, not just the
outcome.
