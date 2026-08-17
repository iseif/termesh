#!/usr/bin/env bash
# Render site/index.html.in into a deployable page.
#
# The page shows the recording and stills from site/img/, which `scripts/record-demo.sh`
# produces by driving a real build with real key chords — regenerated rather than redrawn,
# which is how the README's old hand-drawn mock came to describe a UI that no longer
# existed.
#
# Output goes to site/_build/, which is generated and not committed.
#
# Usage:
#   ./scripts/build-site.sh          # render
#   ./scripts/build-site.sh --open   # and open it in a browser (macOS)

set -euo pipefail

log() { printf '\033[1;35m▶ %s\033[0m\n' "$*"; }
fail() { printf '\033[1;31m✗ %s\033[0m\n' "$*" >&2; exit 1; }

cd "$(dirname "${BASH_SOURCE[0]}")/.."
[[ -f site/index.html.in ]] || fail "site/index.html.in is missing"

OUT="site/_build"

# Read the released version rather than carrying a second copy of it.
VERSION="$(sed -n '/^\[workspace.package\]/,/^\[/s/^version = "\(.*\)"/\1/p' Cargo.toml)"
[[ -n "$VERSION" ]] || fail "no workspace version in Cargo.toml"

mkdir -p "$OUT"

VERSION="$VERSION" OUT="$OUT" python3 - <<'RENDER'
import os

page = open("site/index.html.in").read()
page = page.replace("{{VERSION}}", os.environ["VERSION"])
if "{{" in page:
    raise SystemExit("unsubstituted placeholders remain in site/index.html.in")
open(f"{os.environ['OUT']}/index.html", "w").write(page)
RENDER

# The recording and stills need a real terminal to produce, which a CI runner rendering
# this page does not have, so they are committed and copied rather than regenerated here.
for asset in site/img/demo.gif site/img/diagnostics.png site/img/git.png; do
  [[ -s "$asset" ]] || fail "$asset is missing — run ./scripts/record-demo.sh"
done
mkdir -p "$OUT/img"
cp site/img/*.gif site/img/*.png "$OUT/img/"

log "wrote $OUT/index.html ($(wc -c < "$OUT/index.html" | tr -d ' ') bytes) for $VERSION"

if [[ "${1:-}" == "--open" ]]; then
  open "$OUT/index.html"
fi
