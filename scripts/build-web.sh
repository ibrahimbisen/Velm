#!/bin/bash
# Build the browser client into web/dist.
#
# `web/` is the source — index.html, selftest.js, probe.html — and `web/dist` is entirely
# generated, which is why it is git-ignored and why nothing may be edited there. Editing
# `web/dist/index.html` and then running this loses the edit, silently, which is exactly the
# kind of thing that costs an hour.
#
# ⚠ The wasm-bindgen CLI's version must match the `wasm-bindgen` crate in Cargo.lock
# *exactly*. A mismatch produces a runtime "invalid import" that reads like an application
# bug, so it is checked here rather than discovered in a browser console.
set -euo pipefail
cd "$(dirname "$0")/.."

want=$(awk '/^name = "wasm-bindgen"$/{getline; gsub(/[",]/,""); print $3; exit}' Cargo.lock)
have=$(wasm-bindgen --version 2>/dev/null | awk '{print $2}' || true)
if [ -z "$have" ]; then
  echo "wasm-bindgen is not installed. Install the version the lock file names:" >&2
  echo "  cargo install wasm-bindgen-cli --version $want" >&2
  exit 1
fi
if [ "$want" != "$have" ]; then
  echo "wasm-bindgen CLI is $have and Cargo.lock says $want." >&2
  echo "  cargo install wasm-bindgen-cli --version $want" >&2
  exit 1
fi

cargo build --profile web --target wasm32-unknown-unknown -p vellum-web
mkdir -p web/dist
wasm-bindgen --target web --no-typescript --out-dir web/dist \
  target/wasm32-unknown-unknown/web/vellum_web.wasm
cp web/index.html web/selftest.js web/chrome.js web/dist/

echo "web/dist ready — $(du -h web/dist/vellum_web_bg.wasm | cut -f1) of wasm"
echo "A board and its pictures are not built, they are exported:"
echo "  cargo run --release -p velmd -- snapshot --board COPY.vellum --out web/dist/board.bin"
echo "  cargo run --release -p velmd -- blobs --board COPY.vellum --blobs BLOBS --out web/dist/blobs"
