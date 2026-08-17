#!/usr/bin/env bash
# Publish the termesh workspace to crates.io, one crate at a time, respecting the
# new-crate rate limit.
#
# Why not `cargo publish --workspace`: crates.io limits *new* crate publishes to a burst
# of 5 and then refills one token every 10 minutes (rust-lang/crates.io,
# src/rate_limiter.rs, LimitedAction::PublishNew). A 16-crate workspace walks straight
# into a 429 on the sixth upload, and the five that already landed are permanent —
# crates.io versions cannot be replaced. So this publishes deliberately and waits.
#
# Resumable by design. Every crate is checked against the registry first, so an
# interrupted run is restarted by running the script again; already-published crates are
# skipped and no token is spent on them.
#
# Usage, from anywhere:
#
#   cargo login                      # once, with a token that owns the termesh-* names
#   ./scripts/publish-crates.sh --dry-run
#   ./scripts/publish-crates.sh
#
# Expect roughly two hours for a first release: five crates immediately, then one per ten
# minutes. A later release publishes *updates*, which are limited far more loosely (burst
# 30, one per minute), so the waiting is close to a one-time cost.
#
# VERSION and CRATES below describe this workspace and have to move with it. Publishing a
# release that added or removed a crate means editing CRATES, in Cargo's own topological
# order — take it from `cargo publish --workspace --dry-run` rather than sorting by hand.

set -euo pipefail

readonly VERSION="0.1.0"
readonly BURST=5
readonly REFILL_SECONDS=600
readonly UA="termesh-release/${VERSION}"

# Cargo's own topological order, taken from `cargo publish --workspace --dry-run`.
# Do not hand-edit: a dependency published after its dependent cannot be verified.
readonly CRATES=(
  termesh-core
  termesh-config
  termesh-filesystem
  termesh-git
  termesh-lsp
  termesh-platform
  termesh-search
  termesh-terminal
  termesh-editor
  termesh-ui
  termesh-workspace
  termesh-agent
  termesh-syntax
  termesh-tasks
  termesh-test-support
  termesh
)

DRY_RUN=0
[[ "${1:-}" == "--dry-run" ]] && DRY_RUN=1

log() { printf '\033[1;35m▶ %s\033[0m\n' "$*"; }
fail() { printf '\033[1;31m✗ %s\033[0m\n' "$*" >&2; exit 1; }

# Publish the workspace this script belongs to, whatever the caller's directory is —
# `cargo publish` is not something to aim at whatever happens to be nearby.
cd "$(dirname "${BASH_SOURCE[0]}")/.."
grep -q '^name = "termesh"$' crates/app/Cargo.toml 2>/dev/null ||
  fail "$(pwd) is not the termesh workspace"

http_status() {
  curl -sS -o /dev/null -w '%{http_code}' -H "User-Agent: $UA" "$1"
}

# Does the crate name exist at all? Note that the /crates/NAME/VERSION endpoint answers
# 404 both for "no such crate" and for "that crate exists but not that version" — it
# cannot be used to tell a free name from someone else's. Only this one can.
crate_exists() {
  local status; status="$(http_status "https://crates.io/api/v1/crates/${1}")"
  case "$status" in
    200) return 0 ;;
    404) return 1 ;;
    *) fail "crates.io returned HTTP $status for $1; refusing to guess" ;;
  esac
}

version_exists() {
  local status; status="$(http_status "https://crates.io/api/v1/crates/${1}/${VERSION}")"
  case "$status" in
    200) return 0 ;;
    404) return 1 ;;
    *) fail "crates.io returned HTTP $status for $1 $VERSION; refusing to guess" ;;
  esac
}

# A name that exists without this version is either a previous release of ours or someone
# else's crate, and the public API cannot tell them apart. Publishing into that ambiguity
# fails deep in the run with a permissions error from cargo; failing here says why.
preflight() {
  local crate taken=()
  for crate in "${CRATES[@]}"; do
    if crate_exists "$crate" && ! version_exists "$crate"; then
      taken+=("$crate")
    fi
  done
  if (( ${#taken[@]} )); then
    printf '\033[1;31m✗ these names exist on crates.io without %s:\033[0m\n' "$VERSION" >&2
    printf '    %s\n' "${taken[@]}" >&2
    fail "confirm you own them (\`cargo owner --list <crate>\`) before publishing"
  fi
}

[[ -z "$(git status --porcelain)" ]] || fail "working tree is dirty; publish an exact commit"

log "publishing $VERSION from $(git rev-parse --short HEAD)"
(( DRY_RUN )) && log "dry run: no uploads, no waiting"

log "checking the names on crates.io"
preflight

spent=0
published=0
for crate in "${CRATES[@]}"; do
  if version_exists "$crate"; then
    log "$crate $VERSION is already on crates.io — skipping"
    # A crate already published means an earlier run spent tokens, and there is no way to
    # ask crates.io how many are left. Assume the bucket is empty: one wasted ten-minute
    # wait is cheaper than a 429 that stops a two-hour unattended job on its first upload.
    spent=$BURST
    continue
  fi

  # The burst covers the first five uploads of the run; after that every upload needs a
  # freshly refilled token. Waiting before the upload rather than after means an
  # interrupted run never leaves a token half-earned.
  if (( spent >= BURST && ! DRY_RUN )); then
    log "rate limit: waiting ${REFILL_SECONDS}s for a token before $crate"
    sleep "$REFILL_SECONDS"
  fi

  if (( DRY_RUN )); then
    log "would publish $crate"
  else
    log "publishing $crate"
    # No --no-verify: the packaged crate must build against the registry copies of its
    # dependencies, which is the only check that the published artifact is usable.
    cargo publish -p "$crate" --locked
  fi
  spent=$(( spent + 1 ))
  published=$(( published + 1 ))
done

log "done — $published crate(s) published this run"
if ! (( DRY_RUN )); then
  printf 'Verify with: cargo install termesh --locked --version %s\n' "$VERSION"
fi
