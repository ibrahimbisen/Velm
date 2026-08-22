#!/usr/bin/env bash
# Builds Velm.app — a double-clickable macOS application.
#
# A bare Cargo binary has no Dock icon, no name in the menu bar, and cannot be
# opened from Finder. macOS wants a bundle: a directory with a known shape and an
# Info.plist telling Launch Services what the thing is called and what it can open.
#
#   ./scripts/make-app.sh            # build and place Velm.app beside the repo
#   ./scripts/make-app.sh /Applications
#   PROFILE=dist ./scripts/make-app.sh /tmp/stage      # the shipping build
#
# Re-run it after any change to the app; it rebuilds the binary first.
#
# `PROFILE` defaults to `release` — thin LTO, which is what the edit loop and the
# `--demo` diagnostics want. **A build for other people should be `dist`**: fat LTO and
# one codegen unit, which measured 19MB against release's 22MB. It takes ~4½ minutes and
# `RULES.md` says to run it alone.

set -euo pipefail
cd "$(dirname "$0")/.."

DEST="${1:-$HOME/Desktop}"
APP="$DEST/Velm.app"
PROFILE="${PROFILE:-release}"

# Three binaries, one invocation — `cargo build` runs the whole workspace's dependency
# graph once, and two invocations would build it twice.
#
# `velm-agent-cli` and `velm-mcp` are not decoration. An agent runs in its own process and
# reaches back into Velm through them: agent-to-agent messages, the shared notes, spawning a
# sub-agent, posting an image, offering the user a choice. `vellum_agent::ipc`'s header has
# always assumed they were on the agent's PATH, and until this line they **were never
# built** — so every one of those features terminated in a command that did not exist.
echo "building the $PROFILE binaries…"
cargo build --profile "$PROFILE" \
  -p vellum-app -p vellum-agent \
  --bin vellum-app --bin velm-agent-cli --bin velm-mcp

mkdir -p "$DEST"
echo "assembling $APP"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "target/$PROFILE/vellum-app" "$APP/Contents/MacOS/Velm"

# Beside the main executable, which is where `vellum_agent::transport::Shim` looks: it
# resolves `std::env::current_exe()`'s directory, so this location and `target/$PROFILE`
# are the same relation and no build layout is encoded in the program. Their names are
# **not** renamed to match the product the way `vellum-app` → `Velm` is — an agent is told
# to run `velm-agent-cli`, and that string is in the system context, the shim's own help and
# `vellum_agent::transport::AGENT_CLI`.
cp "target/$PROFILE/velm-agent-cli" "$APP/Contents/MacOS/velm-agent-cli"
cp "target/$PROFILE/velm-mcp" "$APP/Contents/MacOS/velm-mcp"

# Inter is compiled into the binary (`vellum_text::BUNDLED_FONTS`), not loaded from here —
# so this copy is the *licence*, not the font. The SIL Open Font License requires the notice
# to travel with the software that carries the faces, and a font linked into an executable is
# still distribution. One file, and the obligation is met wherever the bundle goes.
cp assets/fonts/Inter-LICENSE.txt "$APP/Contents/Resources/Inter-LICENSE.txt"

# The mark is four corner brackets around a frame that is never drawn, one corner
# in xr-red. Drawn here rather than converted from assets/logo/mark.svg because
# macOS has no SVG rasteriser on the command line, and a hand-drawn icon at each
# size beats one blurry upscale.
python3 - "$APP" <<'PY'
import sys, pathlib
from PIL import Image, ImageDraw

app = pathlib.Path(sys.argv[1])
iconset = app / "icon.iconset"
iconset.mkdir(parents=True, exist_ok=True)

# Kept in step with `vellum_ui::theme::swatch::light` by hand — this runs under
# Python, so there is no way to read the Rust constants. `mark.rs`'s tests pin the
# SVG against the palette; this is the third copy and the one nothing can check, so
# a change to the ramp or the accent has to come here too.
# *"in the app logo make the background white"*. Was pearl (235, 238, 240) — "the
# canvas the mark sits on", which read as a grey tile in the Dock beside icons that
# are mostly white. This is the **only** white in the icon: the bracket mark supplies
# all of the contrast, so the tile has no hairline to keep visible and nothing is lost
# by taking it to full value. It does not follow `bone`, deliberately — see
# `vellum_ui::theme::swatch::light::GLASS_WHITE` for the same split on the glass tint.
GROUND = (255, 255, 255, 255)   # white — the tile the mark sits on
INK    = (26, 29, 31, 255)      # ink
ACCENT = (0, 163, 140, 255)     # signal-teal

def draw(px: int) -> Image.Image:
    # Supersample and downscale: PIL has no antialiased line drawing, and the
    # mark is nothing but lines, so aliasing would be the whole icon.
    s, u = 8, px * 8
    img = Image.new("RGBA", (u, u), (0, 0, 0, 0))
    d = ImageDraw.Draw(img)

    # macOS rounds every icon to the same squircle; approximating with a rounded
    # rectangle at ~22% is close enough that the difference is invisible at
    # Dock sizes.
    d.rounded_rectangle([0, 0, u - 1, u - 1], radius=int(u * 0.22), fill=GROUND)

    # Grid 48, inset 8, arm 12 — the proportions in assets/logo/README.md, scaled.
    k = u / 48.0
    w = max(1, int(2.5 * k))
    def seg(pts, colour):
        d.line([(x * k, y * k) for x, y in pts], fill=colour, width=w, joint="curve")

    seg([(8, 20), (8, 8), (20, 8)], ACCENT)      # top-left, accented
    seg([(28, 8), (40, 8), (40, 20)], INK)
    seg([(40, 28), (40, 40), (28, 40)], INK)
    seg([(20, 40), (8, 40), (8, 28)], INK)

    return img.resize((px, px), Image.LANCZOS)

# Every size Launch Services asks for, including the @2x variants.
for size in (16, 32, 128, 256, 512):
    draw(size).save(iconset / f"icon_{size}x{size}.png")
    draw(size * 2).save(iconset / f"icon_{size}x{size}@2x.png")
PY

iconutil -c icns "$APP/icon.iconset" -o "$APP/Contents/Resources/Velm.icns"
rm -rf "$APP/icon.iconset"

cat > "$APP/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key>              <string>Velm</string>
  <key>CFBundleDisplayName</key>       <string>Velm</string>
  <key>CFBundleExecutable</key>        <string>Velm</string>
  <key>CFBundleIdentifier</key>        <string>app.velm.Velm</string>
  <key>CFBundleIconFile</key>          <string>Velm</string>
  <key>CFBundlePackageType</key>       <string>APPL</string>
  <!-- Both of these are placeholders and are overwritten a few lines below from
       `[workspace.package] version` in Cargo.toml, which is the one place a version
       number is written. Without that the About dialog (`env!("CARGO_PKG_VERSION")`)
       and Finder would read two different numbers, and nothing here compares them.
       A bundle reporting 0.0.0 names the step that did not run; a real-looking 1.1.0
       left here would go stale in silence, which is the trap `locked: false` and
       `THEME_BORDER` have each already cost once. -->
  <key>CFBundleShortVersionString</key><string>0.0.0</string>
  <key>CFBundleVersion</key>           <string>0.0.0</string>
  <key>LSMinimumSystemVersion</key>    <string>11.0</string>
  <key>NSHighResolutionCapable</key>   <true/>
  <!-- The canvas is drawn by us at every zoom, so macOS must not scale the
       window's backing store; that would make text soft on a Retina display. -->
  <key>NSSupportsAutomaticGraphicsSwitching</key><true/>
  <key>CFBundleDocumentTypes</key>
  <array>
    <dict>
      <key>CFBundleTypeName</key><string>Velm Board</string>
      <key>CFBundleTypeRole</key><string>Editor</string>
      <key>LSItemContentTypes</key><array><string>app.velm.board</string></array>
    </dict>
  </array>
  <key>UTExportedTypeDeclarations</key>
  <array>
    <dict>
      <key>UTTypeIdentifier</key><string>app.velm.board</string>
      <key>UTTypeDescription</key><string>Velm Board</string>
      <key>UTTypeConformsTo</key><array><string>public.data</string></array>
      <!-- `vellum`, which is what a board file actually is — see
           `vellum_store::BOARD_EXTENSION`. This said `vellm` (now `velm`), the
           extension of the *backup* export, so double-clicking a board in Finder
           had never been associated with this app at all. The crate prefix and the
           product name are different words and this is the one place it mattered.

           The `UTTypeTagSpecification` key below is load-bearing far beyond the
           file association: the rename that fixed the extension deleted the key
           and re-added only its `<dict>`, leaving a bare `<dict>` where a `<key>`
           belongs. That is invalid plist XML, so macOS discarded the *whole file*
           — `CFBundleIconFile` with it — and the app shipped with a blank icon.
           Hence the `plutil -lint` below: nothing else caught it. -->
      <key>UTTypeTagSpecification</key>
      <dict><key>public.filename-extension</key><array><string>vellum</string></array></dict>
    </dict>
  </array>
</dict>
</plist>
PLIST

# The plist is hand-written XML in a heredoc, so a typo in it is a typo nothing else
# in this build reads. macOS does not complain about a malformed one — it silently
# ignores the file and falls back to a blank icon and the folder's name, which is
# indistinguishable from "the icon did not get built". `set -e` turns that into a
# failed build instead.
plutil -lint "$APP/Contents/Info.plist"

# The version, stamped from the one place it is written. The heredoc above is quoted
# (`<<'PLIST'`) on purpose so that nothing in that XML is at the mercy of the shell,
# and unquoting it to interpolate `$VERSION` would put every `$` in the document at
# risk for one substitution. `plutil -replace` edits the finished file instead.
VERSION="$(awk '/^\[workspace\.package\]/{f=1} f && /^version = /{gsub(/"/,"",$3); print $3; exit}' Cargo.toml)"
test -n "$VERSION"
plutil -replace CFBundleShortVersionString -string "$VERSION" "$APP/Contents/Info.plist"
plutil -replace CFBundleVersion            -string "$VERSION" "$APP/Contents/Info.plist"

# Read it back rather than trusting the write. `plutil -replace` on a key that is not
# there succeeds by adding it, so a renamed key would leave the real one at 0.0.0 and
# report nothing at all.
test "$(plutil -extract CFBundleShortVersionString raw "$APP/Contents/Info.plist")" = "$VERSION"
test -f "$APP/Contents/Resources/Velm.icns"

# The agent shims, checked as executables rather than as files. A `cp` of something the
# build did not produce fails under `set -e` above, so this is really asking the question
# the app asks at runtime: `Shim::beside` requires the execute bit, and a bundle whose
# shims are not runnable degrades silently into agents that cannot reach the board.
test -x "$APP/Contents/MacOS/velm-agent-cli"
test -x "$APP/Contents/MacOS/velm-mcp"

# Unsigned bundles are quarantined when they arrive from anywhere but the local
# filesystem. Ad-hoc signing keeps Gatekeeper quiet for a locally built app.
#
# Not muted. `2>/dev/null || true` used to hide the "Info.plist=not bound" that a
# malformed plist causes, which was the one signal that would have named the bug
# above. A signing failure is worth seeing even though it is not worth stopping for.
codesign --force --deep --sign - "$APP" || echo "warning: ad-hoc signing failed" >&2

echo
echo "built $APP"
du -sh "$APP" | awk '{print "size: "$1}'
echo
echo "open it from Finder, or:  open '$APP'"
