#!/usr/bin/env bash
# Signs a built Velm.app with a real Developer ID, has Apple notarize it, staples the
# ticket to it and wraps the result in a drag-to-install DMG.
#
#   VELM_SIGN_IDENTITY="Developer ID Application: Name (TEAMID)" \
#     ./scripts/sign-release.sh /tmp/stage/Velm.app /tmp/Velm-1.2.0-macos-arm64.dmg
#
# This is deliberately separate from `make-app.sh`, which ad-hoc signs (`--sign -`).
# Ad-hoc is right for a local build: it keeps Gatekeeper quiet for a bundle that never
# leaves the machine, and it needs no certificate. It is worthless for a download —
# an ad-hoc signature has no identity behind it, so macOS refuses the app on any other
# Mac and the only way in is the `xattr -dr com.apple.quarantine` incantation.
#
# Notarization is what removes that. It is *not* a second signature: Apple's service
# scans the binary, and a "ticket" saying it passed is stapled to the bundle so the
# check works offline afterwards.
#
# ## Credentials
#
# Signing needs `VELM_SIGN_IDENTITY` and a matching certificate in the keychain.
# Notarizing needs an App Store Connect API key, given either as a file or base64:
#
#   APPLE_API_KEY_P8       base64 of the .p8   (what CI passes, from a GitHub secret)
#   APPLE_API_KEY_P8_FILE  path to the .p8     (what a person passes locally)
#   APPLE_API_KEY_ID       the 10-character Key ID
#   APPLE_API_ISSUER_ID    the issuer UUID
#
# **With no key present the script signs, verifies and builds the DMG, and skips
# notarization, saying so.** That is the local dry run, and it is the only way to find
# out whether the hardened runtime breaks the app *before* spending a CI cycle on it.

set -euo pipefail

APP="${1:-}"
DMG="${2:-}"

if [[ -z "$APP" || -z "$DMG" ]]; then
  echo "usage: $0 <path/to/Velm.app> <path/to/output.dmg>" >&2
  exit 2
fi
if [[ ! -d "$APP" ]]; then
  echo "error: no bundle at $APP — run scripts/make-app.sh first" >&2
  exit 1
fi

IDENTITY="${VELM_SIGN_IDENTITY:-}"
if [[ -z "$IDENTITY" ]]; then
  echo "error: VELM_SIGN_IDENTITY is not set." >&2
  echo "       Your identities:" >&2
  security find-identity -v -p codesigning >&2 || true
  exit 1
fi

# A name is not a key. Two Developer ID certificates for the same team have the *same*
# common name, and `codesign` then refuses with "ambiguous (matches ... and ...)" — which
# is exactly what a machine holding a renewed certificate beside the original looks like.
# The SHA-1 hash `security find-identity` prints is unambiguous, and codesign takes it in
# place of the name. Checked here rather than left to codesign, because by the time
# codesign says it the version has already been bumped, committed, tagged and pushed.
if [[ ! "$IDENTITY" =~ ^[0-9A-Fa-f]{40}$ ]]; then
  matches="$(security find-identity -v -p codesigning \
             | awk -v want="$IDENTITY" 'index($0, want) { print $2 }' | sort -u)"
  count="$(printf '%s\n' "$matches" | grep -c . || true)"
  if [[ "$count" -eq 0 ]]; then
    echo "error: no codesigning identity matches \"$IDENTITY\"." >&2
    security find-identity -v -p codesigning >&2 || true
    exit 1
  fi
  if [[ "$count" -gt 1 ]]; then
    echo "error: \"$IDENTITY\" matches $count certificates, so codesign cannot choose." >&2
    echo "       Set VELM_SIGN_IDENTITY to one of these hashes instead:" >&2
    printf '         %s\n' $matches >&2
    exit 1
  fi
fi

WORK="$(mktemp -d)"
KEYFILE=""
DECODED_KEY=""     # set only for a .p8 this script wrote itself
cleanup() {
  # A decoded .p8 is a credential, so it is overwritten before it is unlinked rather
  # than trusting a delete to be a delete on a copy-on-write filesystem.
  #
  # Only ever the copy *this script made*. `APPLE_API_KEY_P8_FILE` points at a key the
  # caller owns and may be the only copy of it — shredding that would destroy the key
  # over a run that merely finished.
  if [[ -n "$DECODED_KEY" && -f "$DECODED_KEY" ]]; then
    dd if=/dev/urandom of="$DECODED_KEY" bs=1k count=8 conv=notrunc 2>/dev/null || true
    rm -f "$DECODED_KEY"
  fi
  rm -rf "$WORK"
}
trap cleanup EXIT

# ---------------------------------------------------------------------------
# 1. Sign, inner binaries first.
#
# Not `--deep`. Apple deprecated it, and it is wrong here for a concrete reason: a
# signature covers everything beneath it, so signing the bundle before its nested
# executables leaves the bundle's own seal describing files that then change. The
# order is inside-out, always.
#
# `--options runtime` is the hardened runtime and it is not optional — the notary
# service rejects any Mach-O in the bundle without it. `--timestamp` asks Apple's
# timestamp authority to countersign, so the signature stays valid after the
# certificate itself expires.
# ---------------------------------------------------------------------------
echo "signing with: $IDENTITY"

# `velm-agent-cli` and `velm-mcp` sit beside the main executable because
# `vellum_agent::transport::Shim` resolves them from `current_exe()`'s directory.
# They are separate Mach-Os and each needs its own hardened-runtime signature.
for shim in velm-agent-cli velm-mcp; do
  echo "  signing $shim"
  codesign --force --timestamp --options runtime \
           --sign "$IDENTITY" "$APP/Contents/MacOS/$shim"
done

echo "  signing the bundle"
codesign --force --timestamp --options runtime \
         --sign "$IDENTITY" "$APP"

# `--strict` because the default is lenient about things the notary service is not.
codesign --verify --strict --verbose=2 "$APP"

# The one line worth asserting: without `runtime` in the flags, notarization is rejected
# minutes from now with a message that does not name this as the cause.
#
# Captured into a variable rather than piped into `grep -q`. `grep -q` exits the moment
# it matches, which closes the pipe and hands codesign a SIGPIPE — and under
# `set -o pipefail` that makes the *successful* case report status 141. Measured: a
# correctly hardened bundle failed this check while `flags=0x10000(runtime)` was on the
# line being tested.
DESCRIBED="$(codesign -dv --verbose=2 "$APP" 2>&1 || true)"
printf '%s\n' "$DESCRIBED" | grep -E 'Authority|TeamIdentifier|flags|Timestamp' || true
case "$DESCRIBED" in
  *"flags="*"runtime"*) ;;
  *)
    echo "error: the bundle is signed but the hardened runtime is not on it." >&2
    exit 1
    ;;
esac

# ---------------------------------------------------------------------------
# 2. Notarize the app, if there is a key. Then staple.
# ---------------------------------------------------------------------------
NOTARIZE=0
if [[ -n "${APPLE_API_KEY_P8_FILE:-}" ]]; then
  KEYFILE="${APPLE_API_KEY_P8_FILE}"
  NOTARIZE=1
elif [[ -n "${APPLE_API_KEY_P8:-}" ]]; then
  KEYFILE="$WORK/AuthKey.p8"
  DECODED_KEY="$KEYFILE"
  ( umask 077; printf '%s' "${APPLE_API_KEY_P8}" | base64 --decode > "$KEYFILE" )
  NOTARIZE=1
fi
if [[ "$NOTARIZE" == 1 && ( -z "${APPLE_API_KEY_ID:-}" || -z "${APPLE_API_ISSUER_ID:-}" ) ]]; then
  echo "error: an API key was given without APPLE_API_KEY_ID / APPLE_API_ISSUER_ID." >&2
  exit 1
fi

notarize() {
  # $1 is the thing to submit. `--wait` blocks until Apple answers; typically under
  # five minutes, occasionally much longer, so the timeout is generous rather than tight.
  xcrun notarytool submit "$1" \
    --key "$KEYFILE" \
    --key-id "$APPLE_API_KEY_ID" \
    --issuer "$APPLE_API_ISSUER_ID" \
    --wait --timeout 45m
}

if [[ "$NOTARIZE" == 1 ]]; then
  # A .app is a directory and cannot be uploaded as one. `ditto -c -k --keepParent`
  # makes the archive the notary service expects; a `zip` from the shell loses the
  # symlinks and extended attributes inside the bundle.
  echo "notarizing the app"
  ditto -c -k --keepParent "$APP" "$WORK/Velm.zip"
  notarize "$WORK/Velm.zip"

  # Stapling writes the ticket *into the bundle*, which is what makes the check work
  # on a machine with no network. The DMG is built from the stapled copy below, so the
  # app a person drags to Applications carries its own ticket.
  xcrun stapler staple "$APP"
  xcrun stapler validate "$APP"
else
  echo "no App Store Connect key given — signing only, NOT notarizing."
  echo "  This build will still show a Gatekeeper warning on another Mac."
fi

# ---------------------------------------------------------------------------
# 3. The DMG: a folder holding the stapled app and a shortcut to /Applications,
#    which is the whole of the drag-to-install gesture.
# ---------------------------------------------------------------------------
echo "building $DMG"
STAGE="$WORK/dmg"
mkdir -p "$STAGE"
# `ditto` rather than `cp -R`: it preserves the signature's extended attributes, and a
# copy that loses them is a bundle that no longer verifies.
ditto "$APP" "$STAGE/$(basename "$APP")"
ln -s /Applications "$STAGE/Applications"

mkdir -p "$(dirname "$DMG")"
rm -f "$DMG"
hdiutil create -volname "Velm" -srcfolder "$STAGE" -ov -format UDZO "$DMG" >/dev/null

# The DMG is signed too, and notarized separately. Stapling the app covers the app once
# it has been dragged out; stapling the disk image is what stops the *download itself*
# being challenged before anyone opens it.
codesign --force --timestamp --sign "$IDENTITY" "$DMG"

if [[ "$NOTARIZE" == 1 ]]; then
  echo "notarizing the disk image"
  notarize "$DMG"
  xcrun stapler staple "$DMG"
  xcrun stapler validate "$DMG"

  # The only check that answers the question a user asks by double-clicking. Anything
  # other than `source=Notarized Developer ID` means they will still see a warning.
  spctl -a -vv -t install "$DMG"
fi

echo
echo "built $DMG"
du -h "$DMG" | awk '{print "size: "$1}'
shasum -a 256 "$DMG"
