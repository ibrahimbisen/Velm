# Architecture

Velm is a native desktop infinite-canvas app for macOS and Windows. It exists because Miro is slow: `/Applications/Miro.app` is **295MB of Electron 38.8.6 / Chrome 140** with 1.5GB of local cache, shipping an entire browser to draw rectangles.

Every decision below is subordinate to one goal: **the canvas must never stutter.** Feature work that would compromise that gets restructured, not accepted.

---

## 1. Pure native. No webview.

An early draft put the renderer inside a Tauri webview, compiling the engine to WASM. That was **reversed**, and the reasoning is recorded because it is the kind of decision that gets re-proposed:

- Tauri [closed WebGPU flag support as "not planned"](https://github.com/tauri-apps/tauri/issues/6381) — there is no supported way to influence the embedded webview's GPU behaviour.
- WKWebView was **frame-capped at 60fps until macOS 26**. The performance target was physically unreachable inside the chosen renderer host.
- WebView2 can silently fall back to software rendering via its GPU denylist, and the fallback is hard to detect from inside WASM.
- WASM costs recur daily: no threads without cross-origin isolation, no system font access, and every decoded image crossing native → IPC → JS → WASM → GPU with at least two copies.

Native `wgpu` talks to Metal and DX12 directly. No cap, no fallback surprise, real `lldb`, ~10MB binaries.

**This does not close the web-viewer door.** `winit + wgpu` also compiles to `wasm32` + WebGPU, and in a real browser — as opposed to an embedded webview — WebGPU works properly. A future viewer reuses the renderer as-is.

## 2. Crate layout

```
crates/
  vellum-import/   Miro clipboard decoder · .rtb reader · SVG oracle       ✅ built
  vellum-doc/      Loro CRDT document, movable-tree hierarchy, undo/redo
  vellum-scene/    R-tree spatial index, camera, culling, hit-testing
  vellum-render/   wgpu: instanced quads, SDF shapes, glyph atlas, textures
  vellum-text/     cosmic-text shaping/layout/editing, IME, styled spans
  vellum-store/    SQLite per board + shared BLAKE3 blob store
  vellum-app/      winit window, input, UI chrome, wiring
```

The dependency direction is strictly downward: `app → {render, scene, doc, store, import}`, `render → scene`, `doc → text`. Nothing depends on `app`.

## 3. The performance argument

Miro degrades as boards grow. The structural fix is that **frame cost must scale with what is on screen, not with what exists**.

| Decision | Why |
|---|---|
| **R-tree spatial index** (`rstar`) | Viewport queries and hit-tests are O(log n + k) in visible items. This is the single most important choice in the codebase. |
| **Instanced GPU draws** | One draw call for all visible quads, not one per widget. |
| **SDF shapes** | Rounded rects, borders and shadows are analytic in a shader — no tessellation, resolution-independent at any zoom. |
| **No garbage collector** | Rust has no GC pause. JS canvas apps stutter under collection precisely while panning, which is the exact moment it is most visible. |
| **f64 world → camera-relative f32** | f32 alone loses precision past ~10⁷; the reference board is already 41,282 × 17,515 px. Rebasing per frame keeps deep zoom stable. |
| **Texture residency** | 205 images decoded to RGBA exceed 1.5GB. Downscale on import, BC7/ASTC compression, LRU eviction keyed on viewport distance. |

**Budgets**, measured against the real imported reference board rather than synthetic quads — 100k untextured rectangles at 120fps is free on any modern GPU and proves nothing:

| Metric | Budget | Miro |
|---|---|---|
| Cold start to interactive | < 300ms | seconds |
| Pan/zoom, real board | 60fps sustained, 120 on ProMotion | degrades |
| Idle memory | < 400MB | multi-GB |
| Installer | ~10–15MB | 295MB |

## 4. Document model — Loro CRDT

`loro` was chosen over Yjs and Automerge: Rust-native, fastest in the crdt-benchmarks B4 suite, and 2–5× smaller encoding.

It is used from day one rather than retrofitted, because it delivers four things at once:

1. **Undo/redo** via its undo manager, correct by construction.
2. **Version history** — a CRDT retains history inherently, so time-travel is nearly free.
3. **Collaboration stays possible** without a rewrite, should it ever be wanted. It is currently **cut** — the user does not use Miro's collaboration features — but Loro syncs peer-to-peer over any transport, so the door is open at no ongoing cost.
4. **Movable tree** for the item hierarchy, modelling frames and groups containing items — and mapping directly onto Miro's `_parent` field, which the importer already produces.

**Text is stored as styled spans, never a plain `String`.** Miro stickies carry rich-text HTML (bold/italic/underline/links), and imports look wrong without it. Retrofitting rich text later would be a format migration, so the model carries spans from the first commit even while only plain text is *editable*.

**Z-order uses fractional indexing** so inserting between two items never renumbers the rest.

## 5. Storage

- **One SQLite DB per board** (`rusqlite`, bundled — no system dependency). Holds the Loro snapshot plus a lightweight index (title, thumbnail, item counts, mtime) so the board library lists 100 boards without loading any of them.
- **A shared content-addressed blob store**, keyed by BLAKE3, outside the per-board DBs. An image used on 20 boards is stored once. Directories are sharded by hash prefix.
- WAL mode; incremental, crash-safe writes.

## 6. Text — the critical path

Acknowledged as the project's largest risk, **4–8 weeks on its own**. `cosmic-text` provides shaping, wrapping and plain-text editing. It does *not* provide rich runs, HTML clipboard paste, IME candidate-window placement anchored to a caret inside a GPU canvas, colour-emoji fallback, or wrap-width parity with Miro.

Mitigation: render rich runs early (rendering is far cheaper than editing), ship plain-text *editing* first, and store spans from day one so nothing needs migrating.

## 6a. UI chrome: egui, but not `egui-wgpu`

Menus, panels and pickers are built with `egui` — immediate-mode, renderer-agnostic, and with real IME support. `vellum-ui` depends on `egui` alone, so the entire chrome builds and unit-tests on a machine with no GPU.

**`egui-wgpu` is deliberately not used.** It pins `wgpu = "29.0"` while this workspace is on 30, and two wgpu versions cannot share a `Device`. Adopting it would couple our graphics stack to egui's release cadence — every future wgpu upgrade gated on someone else's release. That is the same failure mode as §1's webview, in miniature, and it is rejected for the same reason.

`vellum-render` draws egui directly instead. egui's paint output is only `Vec<ClippedPrimitive>` — position/uv/colour triangles — plus a `TexturesDelta` for the font atlas. Both already have pipelines. A few hundred lines buys permanent version independence, and the chrome shares the canvas's device, queue and frame.

`egui-winit` is used as-is: it targets winit 0.30, the same line as the rest of the workspace, and handles the platform input and IME plumbing that is genuinely tedious to redo.

## 7. Deliberate limitations

- **Live embeds** (YouTube, Figma, Google Docs) cannot render inside a GPU canvas. They import as cards with title, description and favicon plus open-in-browser. Live rendering would need a `wry` webview overlay — deferred, and the reason is §1.
- **No cloud anything, and no collaboration at all.** No account, no sync service, no telemetry, no presence, no cursors, no comments. The user does not use these; building them would be waste.
- **`.rtb` board content is unreadable.** Miro encrypts it server-side; see `docs/02-miro-formats.md`. The clipboard route supersedes it and carries *more*, including pen drawings that no Miro API exposes.

## 8. Testing

- **Golden-image render tests** per widget type.
- **Round-trip property tests** — save → load → identical document.
- **Loro convergence tests** on concurrent edits — cheap insurance that keeps the collaboration door open even though it is not being built.
- **Import fidelity against the SVG oracle**: a clipboard import of the reference board must reproduce 44 stickies (43 `#fff79e`, 1 `#ff9e9e`), 12 frames, 47 shapes, 18 connectors, 134 ink paths, 429 text strings. The two formats come from different Miro code paths, which makes this the strongest correctness signal available.
- **CI builds and smoke-tests macOS + Windows from Slice 0**, so Windows never becomes a late surprise.
