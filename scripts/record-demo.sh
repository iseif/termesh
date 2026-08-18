#!/usr/bin/env bash
# Record the landing-page demo GIF with VHS (https://github.com/charmbracelet/vhs).
#
# The recording is scripted rather than captured by hand for the same reason the landing
# page renders its frames at build time: a demo that cannot be regenerated goes stale
# silently, and a stale demo of an IDE is worse than none.
#
# The project being edited is built here, from scratch, on every run — so the recording
# never contains a real path, branch name, or anything else from the machine that made it.
#
# Requires: vhs (brew install vhs), and a release build of termesh.
#
# Usage:
#   ./scripts/record-demo.sh

set -euo pipefail

log() { printf '\033[1;35m▶ %s\033[0m\n' "$*"; }
fail() { printf '\033[1;31m✗ %s\033[0m\n' "$*" >&2; exit 1; }

cd "$(dirname "${BASH_SOURCE[0]}")/.."
ROOT="$PWD"

command -v vhs >/dev/null || fail "vhs is not installed (brew install vhs)"
[[ -f site/demo.tape ]] || fail "site/demo.tape is missing"

BIN="$ROOT/target/release/termesh"
# Always, never "only if missing". This script exists to produce evidence about what the
# code does, and a stale binary makes it produce evidence about what the code used to do —
# which is worse than no recording, because it looks exactly as convincing. Cargo is
# incremental, so this costs a second when nothing has changed.
log "building termesh (release)"
cargo build --release --locked -p termesh

# A fixed location, so the tape can name it and the recording shows a short, neutral path.
PROJECT="${TMPDIR:-/tmp}/termesh-demo"
rm -rf "$PROJECT"
mkdir -p "$PROJECT/src"

log "creating the demo project at $PROJECT"

cat > "$PROJECT/Cargo.toml" <<'TOML'
[package]
name = "orders"
version = "0.1.0"
edition = "2021"
TOML

cat > "$PROJECT/src/main.rs" <<'RUST'
mod cart;
mod pricing;

fn main() {
    let cart = cart::Cart::new();
    println!("total: {}", pricing::total(&cart));
}
RUST

cat > "$PROJECT/src/cart.rs" <<'RUST'
#[derive(Default)]
pub struct Cart {
    pub items: Vec<Item>,
}

pub struct Item {
    pub name: String,
    pub pence: u32,
    pub quantity: u32,
}

impl Cart {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&mut self, name: &str, pence: u32, quantity: u32) {
        self.items.push(Item { name: name.to_string(), pence, quantity });
    }
}
RUST

# The deliberate mistake the demo walks into: `subtotal` returns a String where the
# signature promises u32, so both cargo and rust-analyzer report it.
cat > "$PROJECT/src/pricing.rs" <<'RUST'
use crate::cart::Cart;

pub fn subtotal(cart: &Cart) -> u32 {
    let pence: u32 = cart.items.iter().map(|item| item.pence * item.quantity).sum();
    format!("{pence}")
}

pub fn total(cart: &Cart) -> u32 {
    subtotal(cart) + vat(cart)
}

pub fn vat(cart: &Cart) -> u32 {
    subtotal(cart) / 5
}
RUST

printf 'target\n' > "$PROJECT/.gitignore"

# Pre-warm before committing, for two reasons: rust-analyzer on a cold target directory
# spends an unpredictable minute indexing, and `cargo check` writes Cargo.lock — which,
# generated after the commit, showed up as an untracked file in the git overlay.
log "pre-warming the demo project (cargo check)"
(cd "$PROJECT" && cargo check --quiet >/dev/null 2>&1 || true)

(
  cd "$PROJECT"
  git init -q
  git config user.email demo@example.com
  git config user.name "termesh demo"
  git add -A
  git commit -qm "orders: cart and pricing"
  # One uncommitted change, so the git status in the recording is not empty.
  printf '\npub fn empty(cart: &Cart) -> bool {\n    cart.items.is_empty()\n}\n' >> src/pricing.rs
)

mkdir -p site/img

# A throwaway config root. `XDG_CONFIG_HOME` covers the session file, agents, settings,
# keymap and crash drafts in one variable — without it the recording restores whatever the
# last run left open, which is exactly how an early take ended up showing a stale buffer.
CONFIG="${TMPDIR:-/tmp}/termesh-demo-config"
rm -rf "$CONFIG"
mkdir -p "$CONFIG/termesh"

# The throwaway config root cannot see the operator's agents.toml, and without an agent
# there is no review to record — which is the one thing here nobody else has.
#
# It has to be an agent that *asks before editing*, or the recording shows a diff for a
# change that already happened. That rules out copying whatever the operator uses day to
# day: this picks a known-gating one deliberately (ADR-0016, docs/support.md).
DEMO_AGENT_CMD="${TERMESH_DEMO_AGENT:-}"
if [[ -z "$DEMO_AGENT_CMD" ]]; then
  if command -v opencode >/dev/null 2>&1; then
    DEMO_AGENT_CMD='"opencode", "acp"'
  else
    fail "no gating agent found. Install opencode, or set TERMESH_DEMO_AGENT to an argv
      JSON fragment for one that asks before editing — e.g. TERMESH_DEMO_AGENT='\"codex-acp\"'.
      See docs/support.md for which agents gate edits."
  fi
fi
cat > "$CONFIG/termesh/agents.toml" <<TOML
[agents.demo]
command = [$DEMO_AGENT_CMD]
TOML
log "agent for the recording: [$DEMO_AGENT_CMD]"

# And the agent has to *ask* before it edits, or the recording shows a diff for a change
# that already happened, which is the opposite of the point (ADR-0016). opencode only asks
# when told to; Codex asks whenever its session is left in read-only.
cat > "$PROJECT/opencode.json" <<'JSON'
{
  "$schema": "https://opencode.ai/config.json",
  "permission": { "edit": "ask", "bash": "ask" }
}
JSON

log "recording (this drives a real terminal; it takes a minute)"
export TERMESH_BIN_DIR="$(dirname "$BIN")"
export DEMO_PROJECT="$PROJECT"
export DEMO_CONFIG="$CONFIG"
vhs site/demo.tape

[[ -s site/img/demo.gif ]] || fail "vhs produced no GIF"
for still in site/img/diagnostics.png site/img/git.png site/img/agent.png; do
  [[ -s "$still" ]] || fail "vhs produced no $still"
done

log "wrote:"
for f in site/img/demo.gif site/img/diagnostics.png site/img/git.png site/img/agent.png; do
  printf '    %-28s %s\n' "$f" "$(du -h "$f" | cut -f1)"
done
