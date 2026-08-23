#!/usr/bin/env bash
#
# Fast in-place update of an installed Velm.app.
#
# For the edit loop: get a code change into the app you are actually running, without
# rebuilding and reinstalling the whole bundle each time.
#
# `build.py` builds the whole bundle from scratch: preflight, icon, `Info.plist`, lint,
# sign, commit, push. That is right for shipping and is a lot of work to repeat when the
# only thing that changed is Rust code. This swaps **just the executable** into the bundle
# that is already on the Desktop and relaunches it.
#
#   ./scripts/update.sh              # update ~/Desktop/Velm.app
#   ./scripts/update.sh /path/to/Velm.app
#
# What it deliberately does NOT do, and why nothing needs deleting:
#
#   - It never removes the bundle. The `Info.plist`, the icon and the bundle identity stay
#     exactly as they were, which is also why Finder does not need its icon cache cleared —
#     that cache is keyed on the bundle *path*, and the path never changes.
#   - It does not touch git. `build.py` commits and pushes; this is for the loop where you
#     are trying something out.
#   - It does not run the tests. Run `cargo test --workspace` yourself when it matters.
#
# ⚠ **It cannot update the icon, the app name or the file associations.** Those live in
# `Info.plist` and `Velm.icns`, which `make-app.sh` generates and this script deliberately
# leaves alone — that is what makes it fast and what keeps Finder's icon cache valid. If
# the icon or the plist changed, run `python3 build.py` once; after that, this script again.
#
# **There is no hot reload, and there cannot be.** This is a compiled binary, so the honest
# floor is one incremental `cargo build`: a few seconds when little changed, up to about two
# minutes cold after `target/` has been cleared. What this removes is everything *around*
# the build.
set -euo pipefail

cd "$(dirname "$0")/.."

APP="${1:-$HOME/Desktop/Velm.app}"
EXE="$APP/Contents/MacOS/Velm"

if [[ ! -d "$APP" ]]; then
  echo "no bundle at $APP" >&2
  echo "run 'python3 build.py' once to create it, then this script keeps it up to date." >&2
  exit 1
fi

echo "▸ building"
# The agent shims travel with the executable. They are versioned with it — they speak
# `vellum_agent::ipc`'s wire format, which is the same crate — so swapping one and not the
# others is how a build ends up with an agent that cannot talk to the Velm that started it.
cargo build --release \
  -p vellum-app --bin vellum-app

# The running app holds its own executable open, and macOS refuses to overwrite a busy
# binary ("Text file busy"). Ask it to quit properly first — a hard kill would skip the
# flush-every-open-board path that `App::shut_down` exists for, and RULE ZERO in CLAUDE.md
# is exactly about not being casual with the user's boards.
if pgrep -x Velm >/dev/null 2>&1; then
  echo "▸ quitting the running Velm (boards flush on quit)"
  osascript -e 'quit app "Velm"' >/dev/null 2>&1 || true
  for _ in $(seq 1 50); do
    pgrep -x Velm >/dev/null 2>&1 || break
    sleep 0.2
  done
  if pgrep -x Velm >/dev/null 2>&1; then
    echo "Velm is still running and will not quit — close it by hand and re-run." >&2
    exit 1
  fi
fi

echo "▸ swapping the executable"
cp target/release/vellum-app "$EXE"

# Beside it, where `vellum_agent::transport::Shim` looks — `current_exe()`'s directory.
# A bundle made by an older `make-app.sh` has neither of these; copying them here is what
# lets this script bring an existing bundle up to date rather than requiring a full rebuild
# for a feature that is only two files.

# Re-sign, or macOS refuses to launch a bundle whose signature no longer matches its
# contents. Ad-hoc, matching `make-app.sh`. Not muted: a signing failure here is the whole
# reason the app would refuse to open, and `make-app.sh` learned that lesson once already
# by ending its codesign with `2>/dev/null || true` and swallowing "Info.plist=not bound".
echo "▸ signing"
codesign --force --deep --sign - "$APP"

echo "▸ launching"
open "$APP"

echo "done — $(du -h "$EXE" | cut -f1) executable swapped into $APP"
