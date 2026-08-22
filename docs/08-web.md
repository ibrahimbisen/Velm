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

## 7. The server, and what it will and will not answer

`velmd serve` puts the three exports behind HTTP. Five routes, every one a `GET`:

    /api/v1/health                    version and liveness — deliberately ungated
    /api/v1/boards                    id, title, item count
    /api/v1/boards/{id}/snapshot      Loro bytes, straight into `Board::from_bytes`
    /api/v1/blobs/{hash}              one picture
    /                                 the client itself

Four decisions worth not re-deriving:

- **A board is named by its file stem and found by *scanning*, never by joining.** A joined
  path needs a traversal check that has to be right; a comparison against stems that came out
  of a directory listing cannot reach anything not already in that directory. `BoardIndex::path`
  is an absolute server path and is never sent.
- **A blob's hash is the traversal defence, and it is total.** `Hash: FromStr` decodes exactly
  64 hex characters, so no spelling of `..` or `/` survives it.
- **⚠ The token gates the boards, not the client.** Getting this wrong cost a build: with the
  gate over everything, the page answered `401` to its own `<script type="module">` import and
  sat on *"Starting…"* for ever. A module fetch and `WebAssembly.instantiateStreaming` fetch
  their own URLs and cannot be handed a header, so a client behind the gate cannot load
  itself. The bundle is the same public wasm anyone can build from the repository; the boards
  are not. It is also why a token may ride in the query string, and why a blob's URL is a base
  **and a suffix** — the token has to land after the hash.
- **A public bind with no token is refused before the socket is bound**, so there is no window
  in which the boards are exposed. The token comes from `$VELMD_TOKEN` and never from a flag,
  which would put it in `ps` and in shell history; it is compared in constant time; and CORS
  names one origin and never `*`, since a wildcard beside `Authorization` means any page on
  the internet can read this person's boards.

**`refuse_live_data` refuses the desktop app's own board directory by name.** `BoardDb::open`
writes a `-wal` sidecar, so serving a directory is not a passive act, and two live SQLite
writers over one board is the one thing that actually corrupts one. Prose has said this in
three places for a while; prose does not stop anybody at one in the morning.

## 8. Measured, on real hardware

Everything below replaces a line that used to read *"unmeasured"*. The board is the user's
own `products` board — 1,306 items, 26,989 × 15,750 world units — served by `velmd` from a
copy, opened in Brave on an M2.

| | |
|---|---|
| Cold start to a drawn frame | **155 ms**, GPU handshake and board fetch included |
| Fitted board, 119 frames | median **10.9 ms (92 fps)**, 95th 39.0 ms, worst 89.0 ms |
| First ten frames | worst **30.9 ms** — shaping and auto-fit, then it settles |
| Steady-state median | **9.4 ms** |
| Bundle | 4.6 MB of wasm, 681 KB brotli'd on the first spike |
| `maxStorageBuffersInVertexStage` | **44** on the user's iPad, 10 on this Mac, against a need of 1 |

`?selftest=perf` produced the frame numbers and reports the **opening** frames separately on
purpose: that is when every visible block is shaped and every auto-fitted sticky is binary
searched, and an average over a hundred frames hides exactly the stall a person notices when
a board opens.

`?selftest=touch` drives five gestures through the page's own listeners and reports what the
camera did — to the status line *and* back to the serving HTTP server, which is what makes it
runnable on the iPad by the person holding it. Measured on the fixed build: one finger 4895.7
world units against 4895.7 wanted, pinch out exactly 3.000×, pinch back in exactly 0.3333×,
two-finger pan 50.0 px with 0.0 px of sideways lurch, a second finger landing mid-drag 1.5 px.
A/B'd against the build before it: **4 of 5 fail**.

**⚠ Two of those five only became discriminating on the second attempt**, and that is the more
useful half. The first version passed the two-finger pan on the broken build, because a shared
drag slot cancels out over a matched pair of moves — the board lands in the right place having
lurched half the fingers' separation and back on every event in between. Sampling *between*
the two moves reports 200.0 px of lurch, the separation exactly. And the second-finger case
passed because it moved the finger that had just landed, which computes against its own
position on either build; moving the *other* finger reports a 284.5 px jump.

## 8a. What the review changed, and the instrument it did not have

Three adversarial reviews over this work returned **17 findings that survived an independent
skeptic each**, six high. Separately the user opened the page and named **four** defects in
one glance. The two sets do not overlap at all, and that is the finding worth keeping:

> Not one of the user's four was in the review, and not one of the review's seventeen was
> visible on screen.

The user's four were all *wrong numbers* in code that compiled, passed its tests and had a
caller — an image size in the wrong units, a text box that was the item's own rectangle, a
frame title that was auto-fitted, a card thumbnail stretched instead of cropped. A reviewer
asks *is this correct*; an auditor asks *can a person get here*; neither looks at the screen.
**A port needs a fourth instrument: the two applications side by side.** `?zoom=`, `?cx=` and
`?cy=` exist so that comparison is against a matched camera rather than eyeballed.

The review's own six high findings, kept because each is a shape rather than a slip:

- **Two leaks a desktop absorbs and a tablet does not** — glyph bitmaps never released on a
  zoom change (wasm linear memory never returns to the OS, so one pinch's peak is permanent),
  and textures uploaded but never evicted.
- **`pointermove` created contacts for pointers that never pressed**, so a hovering Pencil
  turned a one-finger pan into a pinch and outlived the gesture.
- **The status badge ate the bottom-left corner** of the board — implicit pointer capture
  meant a thumb resting there sent its whole stream to a `<div>`.
- **A slowloris before the token gate** — a per-*read* timeout is not a deadline.
- **A RULE ZERO guard that disabled itself** whenever `$HOME` was unset, which is every
  systemd unit. A guard whose failure mode is *permit* is not a guard.

## 9. What is built, what is not, and what is merely untested

⚠ **This section is a ledger with a date on it, and `CLAUDE.md` feedback 37 is the reason to
say so: a ledger written by the session that built the thing was wrong in both directions.**
Everything below was re-checked on 2026-08-22 by running it, not by reading it.

### Built and measured

- **Every item kind on a real board draws with its own geometry.** Ink, connectors, all 41 SDF
  shapes, images, text, and — new — tables, charts, mind maps and kanban boards. Verified by
  photographing the four widget demos in Brave beside the desktop app's own render of the same
  board files: the bar/line/area/donut charts are indistinguishable, the kanban carries its
  WIP limits, the mind map draws all four layout forms, the table auto-fits its columns.
- **Per-run text styling survives**, through `vellum_project::runs::convert` — the same
  conversion the desktop painter makes, in the crate both front ends share.
- **The ↗ badge and the ▶ play button**, drawn *and* pressable. Measured on the `links`
  fixture: ↗ on all five cards, ▶ on exactly one — the video — which is `plays_video` being a
  host list rather than "has a thumbnail".
- **Frame clipping**, in both the geometry and the text pass, from
  `vellum_project::frame::clipped_by_frame`. Before this the two applications drew different
  boards: an item dragged off a frame was hidden on the Mac and still drawn in a tab.
- **A board picker** at `boards.html`, which is the front door that did not exist.
- **Two-way sync.** Measured three ways against copies with a scratch `HOME`: 41 shapes placed
  on the Mac took the server from 1089 to 1130 items; a stale copy synced up to 1130; and a
  live Brave tab merged an edit made on the Mac with no reload — `velm sync: merged 1
  update(s)`, fourteen times as the edits streamed in.

### Deliberately not built

- **The client never sends.** It receives changes and merges them; there is no delta parameter
  anywhere in `live.rs`, so read-only is structural rather than conventional. This is the RULE
  ZERO posture and not a gap to close casually — a tab the OS can kill with no flush, holding
  the only recent copy of an irreplaceable board, is exactly what RULE ZERO forbids.
- **The Agent Canvas is absent**, deferred by the user's own direction.

### Untested rather than working

- **iPad jetsam survival is unmeasured.** wasm linear memory never returns to the OS, so peak
  becomes permanent, and Safari can kill a background tab with no warning.
- **`velmd` has been run on macOS only.** Nothing in it is platform-specific and it is meant
  for Linux, but "meant for" is not "measured on".
- **Nothing has been served over HTTPS from a real domain.** Every measurement here is
  loopback, which is a secure context by exemption — so the one thing the whole hosting guide
  turns on has not been exercised.
- **The `?token=` in a board link lands in browser history.** A navigation cannot carry a
  header, which is why the query form exists at all. `boards.html` scrubs it out of the
  address bar with `replaceState`; it cannot scrub the entry.

### One bug worth remembering, because of how it hid

⚠ **`velmd` never percent-decoded a board id, so nearly every board answered 404.** A browser
sends `encodeURIComponent`, so *"BMW 2020 530i g30"* arrived as `BMW%202020%20530i%20g30` and
matched nothing. Measured before the fix: **404** on the encoded id, **200 and 931,984 bytes**
on a one-word stem of the same board file. It survived because the development board had a
one-word name — so the failure looked like a problem with *particular boards* rather than with
every name that has a space in it, which is most of this user's. It was found by tracing
sync's URL, not by using the client.
