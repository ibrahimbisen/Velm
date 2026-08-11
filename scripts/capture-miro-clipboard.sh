#!/usr/bin/env bash
# Saves the current clipboard's HTML flavour to a file, then decodes it.
#
# Miro clipboard payloads are the only readable source of board structure, and the
# clipboard is volatile — anything else that copies destroys the sample. Capture
# first, analyse afterwards.
#
#   ./scripts/capture-miro-clipboard.sh                     # timestamped capture
#   ./scripts/capture-miro-clipboard.sh reference-board      # named capture
#
# Captures land in captures/ (git-ignored; they contain real board content).
# Promote a small one into crates/vellum-import/tests/fixtures/ to lock in a
# regression test.

set -euo pipefail
cd "$(dirname "$0")/.."

name="${1:-clipboard-$(date +%Y%m%d-%H%M%S)}"
out="captures/${name}.html"
mkdir -p captures

# `the clipboard as «class HTML»` prints «data HTML<hex>»; unwrap to raw bytes.
# Read-only: never write the clipboard back, or the payload under test is lost.
if ! raw="$(osascript -e 'the clipboard as «class HTML»' 2>/dev/null)"; then
  echo "error: the clipboard has no HTML flavour." >&2
  echo "       In Miro, select objects and press Cmd+C, then re-run this." >&2
  exit 1
fi

python3 -c '
import sys
raw = sys.stdin.read().strip()
if not raw.startswith("«data HTML"):
    sys.exit("error: unexpected AppleScript output: " + raw[:60])
sys.stdout.write(bytes.fromhex(raw[len("«data HTML"):-1]).decode("utf-8", "replace"))
' <<<"$raw" >"$out"

bytes=$(wc -c <"$out" | tr -d ' ')
echo "captured $bytes bytes -> $out"

if ! grep -q 'miro-data-v' "$out"; then
  echo
  echo "warning: no Miro marker in this capture — it is HTML from some other app." >&2
  echo "         Copy inside Miro (Cmd+A then Cmd+C) and re-run." >&2
  exit 1
fi

echo
cargo run -q --bin miro-peek -- "$out"
