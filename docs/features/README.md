# Feature catalogue — full Miro parity

The target is **everything you can do on a Miro board**, on your boards, offline. This page is the complete inventory: every Miro capability, how Velm implements it, and which phase it lands in. If something is missing from this list, it is an oversight — say so and it gets added.

**Two deliberate departures from Miro:**

1. **Collaboration is cut, not deferred.** The user does not use Miro's collaboration features at all, so there is no presence, no live cursors, no comments, no voting, no sync — LAN or otherwise. This removes an entire phase. Loro is still the document layer, because it earns its place on undo/redo and version history alone; it also means collaboration remains *possible* later without a rewrite, if that ever changes.
2. **No marketplace apps or third-party integrations** (Jira, Asana, Slack…). These are cloud services; the local equivalents that matter — timer, voting, estimation — are built in as first-class tools.

> **Building any UI? Read [`../04-ui-reference.md`](../04-ui-reference.md) and [`../05-design-language.md`](../05-design-language.md) first.** It transcribes Miro's actual toolbar, menus, shape picker, board library and interaction conventions from screenshots of a real account. The user has years of muscle memory in Miro; controls belong where their hands already reach.

**Phase key** — `P1` usable board · `P2` Miro import · `P3` navigation & output · `P4` advanced widgets · `P5` AI.

**Status key** — ✅ built · 🔨 in progress · ⬜ planned.

---

## 1. Canvas objects

| Feature | Miro behaviour | Velm implementation | Phase | Status |
|---|---|---|---|---|
| Sticky notes | Colour packs, auto-fit text, author attribution | Instanced quad + SDF rounded corners; text auto-fit via binary search on cosmic-text layout | P1 | ⬜ |
| Sticky bulk mode | Type many stickies rapidly, one per line | Paste-splitting on newline + a bulk entry mode | P4 | ⬜ |
| Sticky packs/stacking | Stacked piles that fan out | Group with a stacked layout mode | P4 | ⬜ |
| Text | Rich text, standalone | cosmic-text + styled spans in Loro | P1 | ⬜ |
| Shapes — basic | Rect, rounded rect, ellipse, triangle, diamond, star, arrow, callout, cross, cylinder, cloud, polygon (5/6/8-gon), parallelogram, trapezoid | SDF shaders for analytic shapes; `lyon` tessellation for the rest. ~10 in P1, full set in P4 | P1/P4 | ⬜ |
| Shapes — flowchart | Process, decision, terminator, data, document, manual input, preparation, delay, storage, display, off-page connector | Same pipeline, one shape descriptor table | P4 | ⬜ |
| Connectors | Straight / elbow / curved, arrowheads both ends, dashed/dotted, labels, anchor points, auto-route, jump-overs | `lyon` stroke tessellation; anchors bind to item ids so they re-route live on drag. Straight+bezier P1, orthogonal routing + jump-overs P4 | P1/P4 | ⬜ |
| Freehand pen | Pressure, colour, thickness | Point capture → Catmull-Rom smoothing → tessellated stroke. **The Miro importer already reads `paint` widgets**, so the data model exists first | P2 | ⬜ |
| Highlighter / marker | Translucent overlay stroke | Same pipeline, multiply blend | P4 | ⬜ |
| Eraser | Stroke and object erase | Hit-test against stroke geometry; split strokes on partial erase | P4 | ⬜ |
| Smart drawing | Sketch snaps to a clean shape | Shape recognition over the point array (`$1` recogniser) | P6 | ⬜ |
| Images | Upload, paste, crop, shape-mask, replace, alt text, borders | GPU textures with mipmaps, BC7 compression, LRU eviction. Crop is a UV-rect, so it stays free | P1 | ⬜ |
| Video | Upload and playback | Native decode to a texture | P4 | ⬜ |
| Documents (PDF) | Multi-page, page nav | `pdfium`/`mupdf` page → texture, cached per page | P4 | ⬜ |
| Embeds | Live iframes (YouTube, Figma, Docs…) | Card with title/description/favicon + open-in-browser. Live rendering needs a webview overlay — deferred deliberately | P4 | ⬜ |
| Frames | Named, aspect presets, act as slides, clip content | Loro movable-tree parent; clipping via scissor rect | P1 | ⬜ |
| Groups | Nest, transform together | Loro movable tree | P1 | ⬜ |
| Cards | Title, description, tags, assignee, due date | Composite widget | P4 | ⬜ |
| Code blocks | Syntax highlighting | `syntect` → styled spans | P4 | ⬜ |
| Icons / stickers | Built-in icon library | Bundled SVG set, tessellated and atlased | P4 | ⬜ |
| Emoji & GIFs | Insert, animated | Colour-emoji font in the glyph atlas; animated GIF as a frame-sequenced texture | P4 | ⬜ |
| Tags | Coloured labels, filterable | Tag set on items + filter index | P3 | ⬜ |

## 2. Structured widgets

| Feature | Velm implementation | Phase | Status |
|---|---|---|---|
| Tables | Rows/columns, merge, header styling, resize. Cells are items in a grid layout container | P4 | ⬜ |
| Mind maps | Node tree + automatic radial/tree layout, collapse/expand | P4 | ⬜ |
| Kanban | Column container with WIP limits; cards reparent on drag | P4 | ⬜ |
| User story mapping | Two-axis grid container | P4 | ⬜ |
| Mockups / wireframes | Bundled UI component shape library | P4 | ⬜ |
| Charts | Bar/line/pie generated from a data table | P4 | ⬜ |
| Timeline / roadmap / Gantt | Time-axis container with bars and dependencies | P4 | ⬜ |
| Docs widget | Long-form rich text block | P4 | ⬜ |
| Slides | Frames sequenced as a deck | P3 | ⬜ |

## 3. Selection & editing

| Feature | Velm implementation | Phase | Status |
|---|---|---|---|
| Click / shift-click select | R-tree hit-test, topmost by z | P1 | ⬜ |
| Marquee & lasso select | Rect and polygon queries against the R-tree | P1 | ⬜ |
| Select all / by type / by colour | Indexed queries | P3 | ⬜ |
| Move, resize, rotate | Transform handles; shift constrains aspect/angle | P1 | ⬜ |
| Multi-select transform | Transform about the group bounding box | P1 | ⬜ |
| Align (6 ways) & distribute (2) | Bounding-box arithmetic over the selection | P4 | ⬜ |
| Tidy up / auto-arrange | Grid packing over the selection | P4 | ⬜ |
| Snapping & smart guides | Candidate edges/centres from the R-tree within a screen-space threshold | P4 | ⬜ |
| Grid + snap-to-grid | Configurable size, quantise on commit | P3 | ⬜ |
| Z-order | Fractional indexing — inserting between two items never renumbers the rest | P1 | ⬜ |
| Lock / unlock | Flag excluded from hit-testing | P1 | ⬜ |
| Group / ungroup | Loro movable tree | P1 | ⬜ |
| Copy / cut / paste / paste-in-place | Native clipboard; **and Miro's format on paste** | P1 | ⬜ |
| Duplicate (Cmd+D), Alt-drag | Clone with offset | P1 | ⬜ |
| Format painter | Copy the style struct between items | P4 | ⬜ |
| Undo / redo, unlimited | Loro undo manager | P1 | ⬜ |
| Find & replace | Full-text index over styled spans | P3 | ⬜ |
| Convert type (sticky ↔ shape ↔ text) | Shared text+style model makes this a kind swap | P4 | ⬜ |
| Edit shape points | Direct path-node editing | P4 | ⬜ |

## 4. Text & styling

| Feature | Velm implementation | Phase | Status |
|---|---|---|---|
| Font family / size / auto-fit | cosmic-text; auto-fit binary-searches the size | P1 | ⬜ |
| Bold / italic / underline / strike | Styled spans — **stored from day one** so this is never a format migration | P1 | ⬜ |
| Text colour & highlight | Span attributes | P1 | ⬜ |
| Alignment, horizontal + vertical | Layout parameters | P1 | ⬜ |
| Lists — bulleted, numbered, checklist | Block-level span attributes | P4 | ⬜ |
| Links | Span attribute + click handling | P2 | ⬜ |
| Line height, letter spacing | Layout parameters | P1 | ⬜ |
| Rich-text editing (caret, selection, IME) | cosmic-text editor. **The project's longest pole — 4–8 weeks** | P4 | ⬜ |
| Fill colour, opacity | Instance attributes | P1 | ⬜ |
| Border colour / width / style / radius | SDF parameters | P1 | ⬜ |
| Shadows | SDF blur | P4 | ⬜ |
| Colour picker + eyedropper | HSV picker; eyedropper reads the framebuffer | P3 | ⬜ |
| Style presets | Named style structs | P4 | ⬜ |

## 5. Navigation

| Feature | Velm implementation | Phase | Status |
|---|---|---|---|
| Infinite pan | Trackpad, scroll, space-drag, middle-drag | P1 | 🔨 |
| Zoom | Wheel, pinch, ±, zoom-to-fit, zoom-to-selection, 100% | P1 | 🔨 |
| Minimap | Downsampled render of board bounds | P3 | ⬜ |
| Frames panel | Ordered frame list, drag to reorder | P3 | ⬜ |
| Content search | Full-text index across items | P3 | ⬜ |
| Filter by type / colour / tag | Indexed queries feeding the cull set | P3 | ⬜ |
| Bookmarks / anchors | Saved camera positions | P3 | ⬜ |
| Presentation mode | Frame-by-frame camera animation, fullscreen | P3 | ⬜ |
| **Board background** | **Solid colour from the palette, dot-grid, line-grid, or plain — selectable per board from Board ▸ Background. Explicitly requested by the user; the canvas backdrop is currently hardcoded.** | **P1** | ⬜ |

## 6. Board organisation

| Feature | Velm implementation | Phase | Status |
|---|---|---|---|
| Board library | SQLite index — title, thumbnail, counts, mtime — without loading documents | P3 | 🔨 |
| Projects / folders | Grouping in the index DB | P3 | ⬜ |
| Templates | Save any board or selection as a template; bundled starter set | P4 | ⬜ |
| Thumbnails | Rendered on save | P3 | ⬜ |
| Cross-board search | Shared full-text index | P3 | ⬜ |
| Version history | Loro is a CRDT — full history and time-travel come free | P4 | ⬜ |
| Duplicate board | Copy DB + reference the same blobs | P3 | ⬜ |
| Favourites / recents | Index metadata | P3 | ⬜ |

## 7. Import & export

| Feature | Velm implementation | Phase | Status |
|---|---|---|---|
| **Miro clipboard import** | Cmd+V pastes a real Miro board | P2 | ✅ decoder |
| **Miro `.rtb` assets** | Original-resolution images joined on `resource.id` | P2 | ✅ reader |
| Miro SVG import | Correctness oracle + image fallback | P2 | ⬜ |
| Miro CSV | Link metadata | P2 | ⬜ |
| Export PNG | Selection / frame / whole board, scale options | P3 | ⬜ |
| Export PDF | Vector, one page per frame | P3 | ⬜ |
| Export SVG | Vector | P3 | ⬜ |
| Export CSV | Sticky and card content | P3 | ⬜ |
| Native `.velm` format | SQLite + Loro snapshot | P1 | 🔨 |
| Print | Via PDF export | P3 | ⬜ |

## 8. Facilitation tools

Built in rather than marketplace apps. Only the ones that are useful **solo** — a timer while you work is worth having; voting and estimation need other people and are therefore cut with the rest of collaboration.

| Feature | Phase | Status |
|---|---|---|
| Timer — countdown, alarm, on-canvas | P4 | ⬜ |

*Voting, estimation, presenter mode, attention management and cursor chat all require other people and are cut with §11.*

## 9. Keyboard & input

| Feature | Velm implementation | Phase | Status |
|---|---|---|---|
| Tool shortcuts (`N` sticky, `T` text, `S` shape, `P` pen, `F` frame, `C` connector, `V` select, `H` hand) | Miro-compatible bindings so muscle memory transfers | P1 | ⬜ |
| Command palette (Cmd+K) | Fuzzy action search | P3 | ⬜ |
| Full remapping | User keymap file | P4 | ⬜ |
| Trackpad gestures | Two-finger pan, pinch zoom, momentum | P1 | 🔨 |
| Stylus / pressure | Pressure-varying stroke width | P4 | ⬜ |
| Accessibility — keyboard nav, screen reader | Focus traversal + platform a11y APIs | P4 | ⬜ |

## 10. AI

Local-first by preference. Anything cloud-backed is opt-in per action and never on by default.

| Feature | Phase | Status |
|---|---|---|
| Cluster & summarise stickies | P6 | ⬜ |
| Prompt → diagram / mind map | P6 | ⬜ |
| Sketch → clean shape | P6 | ⬜ |
| Board summary | P6 | ⬜ |

## 11. Collaboration — cut

**Not built, and not planned.** The user does not use Miro's collaboration features, so there is no presence, no live cursors, no comment threads, no mentions, no reactions, no voting, no estimation, no presenter/follow mode, no Talktrack, no sharing, no permissions, and no sync of any kind — cloud or LAN.

This removes an entire phase of work.

The document layer is still `loro`, a CRDT, because it earns its place on **undo/redo and version history** alone. A useful side effect is that adding collaboration later would be a transport problem rather than a rewrite — but that is a door left open, not a plan.

---

## Honest scope note

This is a large surface — Miro is a decade of work by a large team. The order above is chosen so the app is genuinely useful long before it is complete: P1 gives a real working board, P2 gets your existing Miro content in, and everything after widens the feature surface. Nothing here is hand-waved as "later" without an implementation approach.

The two known long poles are **rich-text editing** (§4, 4–8 weeks alone) and **image memory management** (§1), both flagged in the plan. Everything else is well-understood work.
