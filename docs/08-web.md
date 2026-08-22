# Velm on the web

Velm compiles to WebAssembly and runs in a browser, with boards held by `velmd` — a server
the user owns. This file records the decisions the way `docs/01-architecture.md` §1 recorded
the webview reversal: so they are not re-litigated, and so the next reader knows which
numbers were measured and which were assumed.

**Everything below marked *measured* was run. Nothing here is recalled.**

---

## 1. Why this does not contradict `docs/01` §1

§1 reversed putting the renderer inside a **Tauri webview**. Two of its four reasons were
specific to an embedded webview and are dead:

- Tauri's closed WebGPU flag is irrelevant when you ship a URL.
- WKWebView's 60fps cap — §1 itself says *"until macOS 26"*, and Safari 26 on iPadOS ships
  WebGPU enabled by default.

§1 line 20 already reserved this: *"winit + wgpu also compiles to `wasm32` + WebGPU, and in a
real browser — as opposed to an embedded webview — WebGPU works properly."*

Two of §1's costs survive and are accepted: **no threads without cross-origin isolation**, and
**no system font access**. One is inverted — `createImageBitmap` decodes on the browser's own
thread pool and uploads without pixels entering wasm memory, which is *one fewer copy than
native*. And one is new since §1 was written, when this was a 7-crate project: **no
subprocesses**, which is what puts the Agent Canvas out of a browser tab.

⚠ **§1 line 20 says "viewer."** Reading it as authorisation for the full app is a widening.
Browser *editing* is a later, separately-decided stage.

## 2. What compiles, measured

`cargo check --target wasm32-unknown-unknown -p <crate> --lib`, one crate at a time.

| Result | Crates |
|---|---|
| **Compile untouched (13)** | `vellum-doc` · `vellum-scene` · `vellum-shapes` · `vellum-ink` · `vellum-connect` · `vellum-table` · `vellum-mindmap` · `vellum-flow` · `vellum-chart` · `vellum-text` · **`vellum-render`** · `vellum-search` · `vellum-export` |
| **Compile with the feature off (3)** | `vellum-link` (`--no-default-features`) · `vellum-agent` (ditto) · **`vellum-ui`** |
| **Do not compile, by design (3)** | `vellum-store` · `vellum-import` · `vellum-app` |

**`vellum-render` needed no change at all**, which is what its own header claimed and nobody
had checked. It requests zero `wgpu::Features`, runs inside `Limits::default()`, and its
`BOARD_SAMPLES = 4` was already chosen because 1 and 4 are the sample counts WebGPU requires
every backend to support. `vellum-text` also passes untouched: Inter is `include_bytes!`'d and
`fontdb`'s system-font scan compiles out to a no-op on wasm.

**`vellum-doc` compiles clean with `loro` and no changes.** That is the keystone: the browser
needs only `from_bytes` / `to_bytes` / `version` / `export_since` / `apply`, five pure
byte-in/byte-out functions, so a wasm client never needs SQLite.

### The three that do not, and why that is correct

- **`vellum-store`** — `rusqlite` with `bundled` compiles C through `cc`. (Measured curiosity:
  on wasm it now resolves to `sqlite-wasm-rs`, whose build script fails.) It does not need to
  compile: **the server runs it, natively and unchanged.**
- **`vellum-import`** — depends on `vellum-store`. Miro import is a server-side job.
- **`vellum-app`** — the remaining work: the `Instant`/`SystemTime` shim, an async
  `Surface::new`, a browser input bridge, and target-scoped dependencies.

## 3. The feature split, and the rule behind it

`vellum-ui` could not compile for wasm because it depends on `vellum-agent`, which depended
unconditionally on `ureq` → `rustls` → `ring`, whose build script does not run for wasm32.
Measured with `cargo tree -i ring`; the chain was exactly
`ring → rustls → ureq → vellum-agent → vellum-ui`.

Two features now exist, **both on by default**, so no native build changes:

- `vellum-link/net` — gates `fetch`, the crate's only socket. `provider` and `meta` (naming a
  site from its host, parsing OpenGraph) are pure string work and stay available, which is
  what lets a card draw before any request finishes.
- `vellum-agent/native` — gates `ipc`, `mcp`, `research`, `ingest`, `transport`, `session`,
  and the capture/transcription half of `voice`.

**The rule: the capability is gated, the vocabulary is not.** `Provider`, `AgentModel`,
`RoleKind`, `Schedule`, `Territory`, `rules`, `notes::slug` and `voice::Preference` compile on
every target, because `vellum-ui` is built from them and the chrome must draw everywhere.
This is `voice::Unavailable`'s own precedent moved one level up — a build without the
capability still has the words to refuse **by name** rather than going quiet.

`voice::Preference` and `voice::NOT_BUILT_IN` moved into `voice_pure.rs`, and
`#[cfg(not(feature = "native"))] pub mod voice` re-exports them, so the public path
`vellum_agent::voice::Preference` resolves identically on both targets and no caller needs a
`cfg` of its own.

⚠ **The opt-in lives in the consumers, not the members.** Cargo refuses `default-features =
false` on a workspace-inherited dependency, so the *workspace root* declares both crates
`default-features = false` and `vellum-app`/`vellum-import` ask for `features = ["native"]` /
`["net"]`. Cargo unifies features across a build graph, so the native build is unchanged.
`velm-agent-cli` and `velm-mcp` carry `required-features = ["native"]`.

## 4. `velmd` and RULE ZERO

The server holds copies. The Mac stays authoritative until desktop sync lands.

**`velmd` never removes a file.** No subcommand, no request, no code path unlinks a `.vellum`.
`crates/velmd/tests/rule_zero.rs` greps the crate's own source for
`remove_file`/`remove_dir_all`/`rename` and fails the build on a hit — **with no exemption for
test code**, which is why the manifest test's scratch helper builds a fresh directory rather
than clearing one. An exemption is a hole, and a hole in this rule is how a board goes missing.

Migration is three commands because each answers a different question and stopping between
them is the point:

- **`manifest`** — a pure file walk with BLAKE3. **It contains no SQLite at all**, and that is
  structural rather than careful: `BoardDb::open` is *not* read-only (it runs
  `CREATE TABLE IF NOT EXISTS`, may bump `user_version`, and WAL mode creates a `-wal`
  sidecar). A tool promising "this only reads" should not be one refactor from breaking it.
- **`verify`** — recompute every hash, diff. Bytes before meaning.
- **`import`** — copies, then opens **the copies** and runs recovery, integrity and load.

*Measured, against the user's real data directory:* **45 boards, 3,519 files, 1.5 GB
fingerprinted in 2.5 seconds**, and `find -mmin -2` afterwards listed nothing — the directory
was not touched, and a `-wal` sidecar present at the time was still byte-identical.

*Measured, full drill on copies of three real boards:* manifest → `rsync -a` → verify
(`0 mismatches · 0 missing`) → import, which opened all three and reported their item counts.

Two traps found by running it rather than reading it:

- **`check_integrity()` answers `Result<bool>`**, and the bool is the answer. `db.check_integrity()?;`
  compiles, discards it, and reports a corrupt board as fine — the exact failure that step exists
  to catch.
- **Zero boards is a failure, not a quiet success.** The first drill copied no boards (a shell
  quoting slip on "Application Support") and `import` printed `0 mismatches` and every other
  reassuring number. `--demo zoom-flicker` fails on `queued == 0` for the same reason. A
  migration is the worst place to learn that lesson twice.

## 5. Hosting: one origin

The app and the boards are served from **the same origin**. That is not a preference; it is
what makes three browser rules stop applying at once — mixed content, Private Network Access
(Chrome 142 enforces it with no fallback), and CORS.

**WebGPU requires a secure context.** `navigator.gpu` is `[SecureContext]`: `https://`,
`http://localhost` and `http://127.0.0.1` qualify; a bare `http://192.168.x.x` does not, and
the result is a page that loads and draws nothing on every browser, with no flag that survives
a restart. A public app site pointing at many private servers is possible, but each server then
needs a real hostname, a real certificate and CORS headers — at which point it is not "closed."

`web/probe.html` measures all of this on the real device before any of it is relied on.

## 6. The probe, run

`web/probe.html`, served over `http://localhost` (a secure context, so `navigator.gpu`
exists), driven headless through Brave on this machine — **adapter `apple / metal-3`**:

| Check | Result |
|---|---|
| `navigator.gpu` | present |
| Adapter + device at `Limits::default()`, zero features | acquired |
| `getPreferredCanvasFormat()` | `bgra8unorm`, and configurable |
| `maxTextureDimension2D` | **16384** (needs 2048) |
| **`maxStorageBuffersInVertexStage`** | **10** (needs ≥ 1) |
| `maxBufferSize` | 4,294,967,292 |
| 4× MSAA, `storeOp: "discard"`, resolve | `rgba(0,163,140,255)` |
| **Triangle drawn from a vertex-stage storage buffer** | `rgba(0,163,140,255)` |
| Uncaptured device errors | 0 |

**Verdict: GO.** The one result that could have doubled the estimate — a vertex-stage storage
buffer, which `mesh.wgsl:15` and `shape.wgsl:33` both need — is not merely permitted at 10
but **draws real pixels**, the exact teal the fragment shader outputs.

This is Apple's Metal driver, which is the same lineage the iPad runs. It is strong evidence
and it is **not** the iPad. Run the page there before treating it as settled.

⚠ **Three bugs in the probe itself, all one shape.** `mapAsync`, `toBlob` and
`createImageBitmap` are each an `await` on a browser callback, and in a headless browser with
no frame loop none of them resolve. The page sat on *"Running…"* forever — which on an iPad is
indistinguishable from the blank-screen failure the probe exists to detect. Every such await
is bounded now, and the page always reaches a verdict. Two further notes worth keeping:
patching only the two that were observed to hang left `toBlob` to hang next (feedback 35's
sibling rule, paid again), and a headless timeout is reported **WARN, not FAIL** — a probe
that says STOP for a reason unrelated to the hardware is one nobody reads to the end.

## 6a. One wasm-only lint, and why it is left alone

`cargo clippy --target wasm32-unknown-unknown` reports `unfulfilled_lint_expectations` on
`vellum-ui/src/dialog.rs:24`'s `#[expect(clippy::large_enum_variant)]`. It is correct and it
is a *consequence* of the feature split: with `native` off, `Dialog::Rules` and
`Dialog::Schedule` carry smaller types, so the enum is no longer large enough to trip the lint
the expectation was written for.

Left as `#[expect]` rather than relaxed to `#[allow]`. The expectation is doing real work on
the target that ships — it will fire natively the day those variants stop being large, which
is the whole point of `expect` over `allow` — and trading that for silence on a target the
lint does not ship to would be optimising the wrong build. **So do not lint the wasm target
with `-D warnings`, and do not "fix" this by weakening the native annotation.**

## 7. Still unproven

Treat these as unknown rather than done:

- **Nothing has yet rendered a board in a browser.** `vellum-render` compiling is not
  `vellum-render` drawing. The probe page and the `vellum-web` spike are what settle it.
- **`maxStorageBuffersInVertexStage` on WebKit is unmeasured.** `vellum-render/src/mesh.rs:270`
  binds a storage buffer to the vertex stage. Core WebGPU allows 8; WebGL2 allows 0; WebGPU
  *compatibility mode* defaults to 0. If Safari answers 0, `mesh.wgsl` and `shape.wgsl` need
  rewriting onto uniform buffers and the estimate roughly doubles.
- **No touch input exists anywhere.** winit's web backend emits `WindowEvent::Touch` and
  nothing else for a finger, so until that arm is written the iPad shows a board that ignores
  being touched.
- **Fitted-board fps, bundle size and iPad jetsam survival are all unmeasured.**
