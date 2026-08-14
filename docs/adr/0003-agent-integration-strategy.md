# 3. Agent integration: ACP-first, tiered, with a free terminal-CLI fallback

Date: 2026-07-29

## Status
Accepted

## Context
The project's entire reason to exist is agent integration. Two questions: (a) which protocol, and (b) how do we avoid the whole project living or dying on a hard integration landing on schedule?

On (a): the industry standardized the editor↔agent interface via the **Agent Client Protocol (ACP)** — an open JSON-RPC 2.0 standard from Zed (Aug 2025), co-maintained with JetBrains, with a shared registry and dozens of compatible agents (Claude Code, Codex, Gemini CLI, OpenCode, Goose). Building a bespoke agent loop would marry us to one vendor and reinvent a solved interface.

On (b): full ACP diff-review integration is real work and could slip.

## Decision
Implement the **client** side of ACP, behind an `AgentService` trait so the wire format is isolated and swappable. Ship integration in **two tiers**:

- **Tier 0 — terminal CLI (free).** Users run any AI CLI inside a managed terminal pane. No agent-specific code; available the moment the terminal exists (Phase 04).
- **Tier 1 — ACP client (the wedge).** Shared project context, inline diff-review of proposed edits (routed through the transaction spine), and permission-gated tool calls. Lands in Phase 03.

Agent-proposed edits are `ChangeSet`s against a base buffer version; on accept we apply or rebase (never direct mutation). Tool calls (run command, out-of-workspace write, network) are permission-gated and shown as argv arrays.

## Consequences
The product is useful even if Tier 1 slips (Tier 0 is nearly free), and Tier 0 is a natural stepping stone. We inherit the ACP agent ecosystem for free and avoid vendor lock. Risk: ACP spec churn — mitigated by isolating it behind `AgentService` and a scripted-agent test harness. If a future need genuinely requires deeper-than-subprocess coupling, that — and only that — would justify revisiting a bespoke layer.
