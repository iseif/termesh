#!/usr/bin/env bash
# The one "definition of done" command. Runs the full gate from CONTRIBUTING.md.
# Usage:
#   ./scripts/verify.sh          # check-only (what CI runs)
#   ./scripts/verify.sh --fix    # auto-format + clippy --fix first, then check
set -euo pipefail
cd "$(dirname "$0")/.."   # always run from the repo root

FIX=0
[ "${1:-}" = "--fix" ] && FIX=1

step() { printf '\n\033[1;35m▶ %s\033[0m\n' "$1"; }

if [ "$FIX" -eq 1 ]; then
  step "fmt (write)"
  cargo fmt --all
  step "clippy --fix"
  cargo clippy --workspace --all-targets --fix --allow-dirty --allow-staged -- -D warnings || true
fi

step "fmt --check"
cargo fmt --all -- --check

step "clippy (deny warnings)"
cargo clippy --workspace --all-targets -- -D warnings

step "test"
cargo test --workspace

step "build"
cargo build --workspace

printf '\n\033[1;32m✓ verify passed — the gate is green\033[0m\n'
