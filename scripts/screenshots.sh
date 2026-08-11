#!/usr/bin/env bash
#
# Regenerate the README screenshots into docs/images/.
#
# Every shot comes from the app's own `--demo` fixtures, never from a real board:
# a board is someone's private work, and the fixtures are reproducible by anyone
# who clones the repo.
#
# ⚠ `HOME=$(mktemp -d)` on every run is NOT optional. `--board /tmp/…` alone is not
# isolation — the app still opens the real `boards/library.json`, and `set_last_board`
# writes the scratch path into it, so the next real launch tries to open a board that
# has been deleted. See Rule Zero in RULES.md. This has happened once already.
#
#   ./scripts/screenshots.sh          # all shots
#   ./scripts/screenshots.sh hero     # one shot by name
#
set -euo pipefail

cd "$(dirname "$0")/.."

BIN=target/release/vellum-app
OUT=docs/images
WIDTH=1600          # README display width; the window renders at 2880 on Retina
SETTLE="${SETTLE:-10}"   # seconds before the app is told to quit. Raise it for shots
                         # whose content arrives over the network — the link cards fetch
                         # their title, icon and poster, and a short settle photographs
                         # three blank cards. `SETTLE=25 ./scripts/screenshots.sh links`.

[ -x "$BIN" ] || { echo "build first: cargo build --release"; exit 1; }
mkdir -p "$OUT"

# name | extra flags
SHOTS=$(cat <<'LIST'
hero|--demo readme --zoom 45
library|--tab 0
context-menu|--demo context-menu
widgets|--demo mindmap --zoom 75
kanban|--demo kanban --zoom 60
table|--demo table --zoom 90
chart|--demo chart --zoom 80
links|--demo links --zoom 70
snapping|--demo snapping --zoom 90
properties|--demo shapes --select-one --show properties --zoom 70
LIST
)

# The board library is the start tab, and a fresh scratch HOME has nothing in it — so
# the honest shot of an empty library is also a useless one. Create a handful of boards
# in the scratch data directory first, each from a different fixture so the cards carry
# different item counts. They must live in the app's own boards directory, not just
# anywhere under HOME, or the library's rescan never sees them.
seed_library() {
    local scratch="$1" boards i=0
    boards="$scratch/Library/Application Support/Vellum/boards"
    mkdir -p "$boards"
    for spec in "release-plan|readme" "shape-catalogue|shapes" "roadmap|kanban" \
                "measurements|chart" "research|mindmap" "specification|table"; do
        HOME="$scratch" "$BIN" \
            --board "$boards/${spec%%|*}.vellum" \
            --demo "${spec##*|}" \
            --exit-after 3 >/dev/null 2>&1 || true
        i=$((i + 1))
    done
    echo "    seeded $i boards"
}

shoot() {
    local name="$1" flags="$2" scratch png
    scratch=$(mktemp -d)
    png="$OUT/$name.png"

    echo "  $name"
    [ "$name" = "library" ] && seed_library "$scratch"
    # `--exit-after` makes the run terminate unattended; the app waits for the asset
    # decode pool to go quiet before it photographs, so give it room.
    HOME="$scratch" "$BIN" \
        --board "$scratch/demo.vellum" \
        $flags \
        --screenshot "$png" \
        --exit-after "$SETTLE" >"$scratch/log" 2>&1 || true

    if [ ! -f "$png" ]; then
        echo "    FAILED — no PNG written. Log:"
        sed 's/^/      /' "$scratch/log" | tail -20
        rm -rf "$scratch"
        return 1
    fi

    sips --resampleWidth "$WIDTH" "$png" >/dev/null 2>&1
    echo "    $(du -h "$png" | cut -f1)  $(sips -g pixelWidth -g pixelHeight "$png" \
        | awk '/pixel/ {printf "%s ", $2}')"
    rm -rf "$scratch"
}

wanted="${1:-}"
while IFS='|' read -r name flags; do
    [ -z "$name" ] && continue
    [ -n "$wanted" ] && [ "$name" != "$wanted" ] && continue
    shoot "$name" "$flags"
done <<<"$SHOTS"

echo
echo "total: $(du -sh "$OUT" | cut -f1)"
