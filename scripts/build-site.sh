#!/usr/bin/env bash
# Render site/index.html.in into a deployable page, substituting real program output.
#
# The frames on the landing page are produced by running the binary at build time rather
# than being pasted in. The README's hand-drawn mock rotted because nothing regenerated it,
# and a landing page is the last place anyone would notice the same drift — so the page
# cannot show a UI that no longer exists.
#
# Output goes to site/_build/, which is generated and not committed.
#
# Usage:
#   ./scripts/build-site.sh          # build the binary if needed, then render
#   ./scripts/build-site.sh --open   # and open it in a browser (macOS)

set -euo pipefail

log() { printf '\033[1;35m▶ %s\033[0m\n' "$*"; }
fail() { printf '\033[1;31m✗ %s\033[0m\n' "$*" >&2; exit 1; }

cd "$(dirname "${BASH_SOURCE[0]}")/.."
[[ -f site/index.html.in ]] || fail "site/index.html.in is missing"

OUT="site/_build"
BIN="target/release/termesh"

if [[ ! -x "$BIN" ]]; then
  log "building termesh (release)"
  cargo build --release --locked -p termesh
fi

VERSION="$(sed -n '/^\[workspace.package\]/,/^\[/s/^version = "\(.*\)"/\1/p' Cargo.toml)"
[[ -n "$VERSION" ]] || fail "no workspace version in Cargo.toml"

mkdir -p "$OUT"

# HTML-escape, because a frame is dropped inside <pre> and the box-drawing characters sit
# next to code samples containing < and &. Trailing spaces go too: they are invisible in a
# terminal but make the <pre> box wider than the frame.
frame() {
  "$BIN" --dump-frame "$@" 2>/dev/null |
    sed -e 's/[[:space:]]*$//' \
        -e 's/&/\&amp;/g' \
        -e 's/</\&lt;/g' \
        -e 's/>/\&gt;/g'
}

log "rendering frames from $("$BIN" --version)"
frame . --lsp-demo > "$OUT/.frame_lsp"

for f in lsp; do
  [[ -s "$OUT/.frame_$f" ]] || fail "the $f frame came out empty"
done

# python3 rather than sed: the frames contain newlines, slashes and ampersands, all of
# which sed substitution would need escaping for.
VERSION="$VERSION" OUT="$OUT" python3 - <<'PY'
import os
out = os.environ["OUT"]
page = open("site/index.html.in").read()
page = page.replace("{{VERSION}}", os.environ["VERSION"])
for name in ("lsp",):
    with open(f"{out}/.frame_{name}") as handle:
        page = page.replace("{{FRAME_%s}}" % name.upper(), handle.read().rstrip("\n"))
leftover = [tag for tag in ("{{VERSION}}", "{{FRAME_LSP}}") if tag in page]
if leftover:
    raise SystemExit(f"unsubstituted placeholders remain: {leftover}")
open(f"{out}/index.html", "w").write(page)
PY

rm -f "$OUT"/.frame_*
# The recording and stills are committed (they need a real terminal to produce, which a
# CI runner building this page does not have), so they are copied rather than regenerated.
mkdir -p "$OUT/img"
cp site/img/*.gif site/img/*.png "$OUT/img/"
log "wrote $OUT/index.html ($(wc -c < "$OUT/index.html" | tr -d ' ') bytes)"

if [[ "${1:-}" == "--open" ]]; then
  open "$OUT/index.html"
fi
