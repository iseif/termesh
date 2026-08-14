# 2. Language and TUI framework: Rust + Ratatui

Date: 2026-07-29

## Status
Accepted

## Context
The hard parts of this project are text-buffer correctness, PTY/terminal emulation, cross-platform behavior, large-file performance, LSP orchestration, and the transaction/proposal spine — not basic widgets.

## Decision
Build in **Rust** with **Ratatui** (Crossterm backend, Tokio runtime). Reuse the strongest native crates: `ropey` (rope), `tree-sitter` (syntax), `alacritty_terminal` (VT parser/grid), `portable-pty` (PTYs), `notify` (watching). Model the buffer/transaction layer on Helix / CodeMirror 6 — the same ecosystem, so its reference architecture is directly applicable.

## Consequences
Higher upfront cost than Go/Bubble Tea or Python/Textual, but a single distributable binary, low latency, and the richest terminal-tool ecosystem. TypeScript/OpenTUI is reserved for a possible future extension SDK, not the core.
