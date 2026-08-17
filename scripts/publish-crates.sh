#!/usr/bin/env bash
# Publish the termesh workspace to crates.io, one crate at a time, respecting whichever
# rate limit actually applies.
#
# Why not `cargo publish --workspace`: crates.io meters publishing with two separate token
# buckets (rust-lang/crates.io, src/rate_limiter.rs). A *new* crate name comes from a
# bucket with a burst of 5 refilling one token every 10 minutes; a *new version of an
# existing crate* comes from a much looser one, burst 30 refilling every minute. The first
# release of this workspace is 16 new names, which walks straight into a 429 on the sixth
# upload — and the five that already landed are permanent, because crates.io versions
# cannot be replaced or re-uploaded.
#
# So the bucket is chosen per crate, from whether the name already exists. A first release
# waits about two hours; every release after it is all updates and needs no waiting at all,
# which is why this must be decided at run time rather than pinned to a constant.
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
# The version comes from the workspace manifest, so releasing is a version bump and a run
# of this script, with no third place to forget. CRATES does have to move with the
# workspace: a release that adds or removes a crate means editing it, in Cargo's own
# topological order — take that from `cargo publish --workspace --dry-run`, not by hand.

set -euo pipefail

# Both buckets, as crates.io defines them. NEW is the one that hurts.
readonly NEW_BURST=5
readonly NEW_REFILL=600
readonly UPDATE_BURST=30
readonly UPDATE_REFILL=60

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

# Read the version being released rather than carrying a copy of it. Every crate inherits
# `version.workspace = true`, so this single line is what they all publish as.
VERSION="$(sed -n '/^\[workspace.package\]/,/^\[/s/^version = "\(.*\)"/\1/p' Cargo.toml)"
[[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+ ]] || fail "no workspace version in Cargo.toml"
readonly VERSION
readonly UA="termesh-release/${VERSION}"
# Who must own every name that already exists. Overridable so the check is not a lie if
# the publishing account ever changes.
readonly OWNER="${CRATES_IO_OWNER:-iseif}"

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

# Every name that already exists has to be one of ours. The first version of this check
# treated "exists without this version" as the danger sign, which is only true for a first
# release — from the second one onwards that describes every crate we own, and the check
# rejected the whole workspace. Ownership is the thing actually being asked about, and
# crates.io serves it without authentication.
owned_by_us() {
  curl -sS -H "User-Agent: $UA" "https://crates.io/api/v1/crates/${1}/owners" |
    grep -Eq "\"login\"[[:space:]]*:[[:space:]]*\"${OWNER}\""
}

preflight() {
  local crate foreign=()
  for crate in "${CRATES[@]}"; do
    if crate_exists "$crate" && ! owned_by_us "$crate"; then
      foreign+=("$crate")
    fi
  done
  if (( ${#foreign[@]} )); then
    printf '\033[1;31m✗ these names exist on crates.io and %s does not own them:\033[0m\n' \
      "$OWNER" >&2
    printf '    %s\n' "${foreign[@]}" >&2
    fail "publishing would fail on permissions; set CRATES_IO_OWNER if the account differs"
  fi
}

[[ -z "$(git status --porcelain)" ]] || fail "working tree is dirty; publish an exact commit"

log "publishing $VERSION from $(git rev-parse --short HEAD)"
(( DRY_RUN )) && log "dry run: no uploads, no waiting"

log "checking the names on crates.io"
preflight

spent_new=0
spent_update=0
published=0
for crate in "${CRATES[@]}"; do
  # Which bucket this upload draws on, decided before it happens. A name that already
  # exists is an update; a name that does not is a new crate.
  if crate_exists "$crate"; then kind=update; else kind=new; fi

  if version_exists "$crate"; then
    log "$crate $VERSION is already on crates.io — skipping"
    # An earlier run published it, spending a token from this same bucket, and crates.io
    # exposes no way to ask how many are left. Assume that bucket is empty. On a first
    # release that costs one wasted ten-minute wait, which is far cheaper than a 429
    # stopping a two-hour unattended job on its first upload; on an update release the
    # cost is a minute.
    [[ $kind == new ]] && spent_new=$NEW_BURST || spent_update=$UPDATE_BURST
    continue
  fi

  if [[ $kind == new ]]; then
    spent=$spent_new burst=$NEW_BURST refill=$NEW_REFILL
  else
    spent=$spent_update burst=$UPDATE_BURST refill=$UPDATE_REFILL
  fi

  # The burst covers the run's first uploads; past it every upload needs a freshly
  # refilled token. Waiting before the upload rather than after means an interrupted run
  # never leaves a token half-earned.
  if (( spent >= burst && ! DRY_RUN )); then
    log "rate limit ($kind): waiting ${refill}s for a token before $crate"
    sleep "$refill"
  fi

  if (( DRY_RUN )); then
    log "would publish $crate ($kind)"
  else
    log "publishing $crate ($kind)"
    # No --no-verify: the packaged crate must build against the registry copies of its
    # dependencies, which is the only check that the published artifact is usable.
    cargo publish -p "$crate" --locked
  fi
  [[ $kind == new ]] && spent_new=$(( spent_new + 1 )) || spent_update=$(( spent_update + 1 ))
  published=$(( published + 1 ))
done

log "done — $published crate(s) published this run"
if ! (( DRY_RUN )); then
  printf 'Verify with: cargo install termesh --locked --version %s\n' "$VERSION"
fi
