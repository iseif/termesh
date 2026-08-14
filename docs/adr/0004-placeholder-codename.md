# 4. Placeholder codename `termide`

Date: 2026-07-29

## Status
Resolved at `0.1.0` — the name is **`termesh`**. The decision below is kept as written, because it
records what was decided in July 2026 and why; the outcome is in *Resolution* at the end.

## Context
Package names, the binary name, and config paths need *a* name to compile and to write docs, but branding isn't decided.

## Decision
Use `termide` as a placeholder: binary `termide`, crates `termide-*`, config under `~/.config/<app>/` in docs. Before the first tagged release, run the naming checklist (ARCHITECTURE.md §21): check GitHub / registries / domains / trademarks; prefer a short, easy-to-type command; avoid names tied to existing editors, shells, or terminals; reserve org + package names + domain together.

## Consequences
A single mechanical rename pass later (crate names, binary, config path, docs). Kept isolated so the rename is cheap.

## Resolution (2026-08-14, Phase 10)

The checklist ran before the first tagged release, as required. **`termide` was already taken**, which
is precisely the outcome the placeholder existed to absorb. The released name is **`termesh`** —
verified free on crates.io, npm, and GitHub before adoption.

The rename cost what this ADR predicted: one mechanical commit. `platform::paths::APP_DIR` was the
single constant behind every config path, so `~/.config/termide/` became `~/.config/termesh/` and the
per-project `.termide/workspace.toml` became `.termesh/workspace.toml` without touching the code that
reads either.

**No config migration ships.** `termide` was never released, so no user has a `~/.config/termide/`
directory to migrate from. Shipping a migration path at `0.1.0` would mean carrying dead code — and a
test suite for it — into the first public release, for a population of zero.

The lesson worth keeping: the placeholder was not overhead. Deferring the name until the checklist
could run is what made discovering `termide` was taken a one-commit problem instead of a release
blocker.
