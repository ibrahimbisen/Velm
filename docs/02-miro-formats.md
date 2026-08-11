# Miro's formats, reverse-engineered

Everything here was derived on 2026-07-28 from one real board — **Reference Board** — exported four ways, plus the installed Miro desktop app. Miro publishes none of this. It is the foundation of Velm's importer, so the evidence is recorded alongside the conclusions: when Miro changes something, this page is what tells us what we actually knew versus what we assumed.

**Summary:** the `.rtb` backup is encrypted and useless for board content. The **clipboard** is not, and carries Miro's full internal widget model — including widget types no Miro API exposes at all. That is the import path.

---

## 1. `.rtb` backup — encrypted, unusable for content

A `.rtb` is a plain ZIP. `unzip` reads it; renaming to `.zip` changes nothing (byte-identical file).

```
meta.json             67 B    {"version":"1.27","encryptionVersion":"1.1","timestamp":1785263296}
board.json           148 B    {"id":-1234567890123456789,"name":"Reference Board", …}
canvas.json      165,536 B    ENCRYPTED  ← every widget, position, connector
resources.json    24,136 B    plaintext asset manifest
plugin_settings.json  22 B    plaintext
showtime.json         15 B    plaintext
tables.json           63 B    ENCRYPTED
<205 assets>                  raw png / jpg / svg / pdf, NOT encrypted
```

### Evidence that `canvas.json` is encrypted, not merely compressed

| Test | Result | Conclusion |
|---|---|---|
| Deflate ratio inside the ZIP | **0%** (`resources.json` next to it: 87%) | Incompressible → high entropy |
| Shannon entropy | **7.9987 / 8.0** | Indistinguishable from random |
| Chi-square over 256 symbols | 300.8 (uniform ≈ 255) | No exploitable bias |
| Byte coverage | 256/256, frequencies 571–712 (ideal 646) | Flat distribution |
| Duplicate 16-byte blocks | **0 of 10,346** | Not ECB |
| Repeating-XOR index of coincidence, key lengths 1–64 | best **1.003** (random 1.0; JSON ≈ 1.8) | Not XOR/Vigenère or any short-key stream cipher |
| Decompression: zlib, raw-deflate, gzip, bz2, lzma, lzma-alone × 100 byte offsets | **0 hits** | Not a compressed container |
| ZIP archive comment / per-entry extra fields | empty | No key material smuggled in metadata |

**Cipher mode.** `canvas.json` is 165,536 bytes (16-aligned) but `tables.json` is 63 (not). Since 165,536 − 63 = 165,473 ≢ 0 (mod 16), the two cannot both be "fixed header + CBC blocks". Consistent with **AES-GCM**: a 12-byte nonce + ciphertext + 16-byte tag = 28 bytes overhead, which admits any plaintext length. XOR-ing each file's first bytes against a presumed `{"` yields different keystreams, confirming a **random per-file nonce**.

### The key is not on the machine

- **Miro.app contains no backup-decryption code.** It does reference `aes-256-cbc`, `createDecipheriv` and `pbkdf2` — but the surrounding strings are `ajv-formats`, `debounce-fn`, `semver`, `onetime`, which are the dependencies of the `conf`/`electron-store` package. That encryption obfuscates the app's own `settings.json`. Unrelated.
- **No cached board content.** `~/Library/Application Support/RealtimeBoard` is 1.5GB, but it is 793MB HTTP cache, 373MB compiled JS and 360MB service worker. Its `IndexedDB` for `https_miro.com` is **68KB** and holds no widgets. Board state arrives over a websocket and lives only in memory.

**Conclusion: unrecoverable.** Restoring a `.rtb` happens on Miro's servers; the key never reaches the client. Independently corroborated by [hx-MiroExporter](https://github.com/spilehx/hx-MiroExporter), which reached the same wall.

**What `.rtb` is still good for:** the **205 unencrypted assets**, at original resolution. `resources.json` maps each asset id to its filename and extension, and assets are stored in the ZIP as `<id>.<ext>`. That `id` is the join key to clipboard `image`/`document` widgets — see §2.

---

## 2. Clipboard — Miro's full internal widget model ✅

Copying objects in Miro places an HTML flavour on the system clipboard containing:

```html
<span data-meta="&lt;--(miro-data-v1)tl2kroutqq+gnq+gn111oZynrq…">
```

The payload is **delimited**, comment-style, not merely prefixed:

```html
<span data-meta="&lt;--(miro-data-v1)  …base64…  (/miro-data-v1)--&gt;">
```

### Decoding

1. Extract the `data-meta` attribute value.
2. Resolve XML entities (`&lt;` → `<`) — needed for the marker to match.
3. Strip the `<--(miro-data-v1)` opening marker. **Version-gate here**: an unrecognised version must fail loudly, never be force-parsed.
4. **Strip the `(/miro-data-v1)` closing marker and any trailing `-->`.** Easy to miss — small copies are truncated before it, so only a full-board copy exposes it. Feeding it into base64 corrupts the payload. Decode **strictly**: a lenient decoder that skips non-alphabet characters silently produces a mangled board instead of an error.
5. Base64-decode (payloads arrive **unpadded**).
6. **Add 197 to every byte, mod 256.**
7. Parse as UTF-8 JSON.

*Verified on a real 1,096,928-byte full-board copy (596 objects): the shift was 197 there too, so it is a fixed constant rather than per-session. **Cmd+A has no capacity cap** — one copy carried the entire board.*

Step 5 is the only obfuscation — a single additive byte shift, no key. `crates/vellum-import/src/clipboard.rs` deliberately does **not** hardcode 197: it tries all 256 offsets and keeps whichever yields JSON with the expected top-level keys. Brute-forcing one byte is free, and it means a change to the constant costs us nothing. The offset actually used is recorded in `Decoded::byte_shift` so a change is visible rather than silent.

### Envelope

```json
{
  "isProtected": false,
  "boardId": "bTBja0JvYXJkSWQ=",
  "version": 2,
  "host": "miro.com",
  "copierType": "COPY",
  "asPortalAmount": …,
  "data": { "objects": [ … ] }
}
```

### Object envelope and reference semantics

Each object: `{ id, initialId, type, meta, widgetData: { type, json } }`.

The object-level `type` is a **container code**, not the widget type:

| Code | Meaning |
|---|---|
| `14` | A widget — carries `widgetData` |
| `10` | A **group** — carries `items: [index, …]` and **no `widgetData` at all** |

**Miro references objects by array index, and `id == index` for every object observed.** This applies to `_parent.index`, connector `primary/secondary.widgetIndex`, and group `items`. Any importer must keep the array order intact.

### Fields on every widget (`widgetData.json`)

| Field | Meaning |
|---|---|
| `_position.offsetPx.{x,y}` | Position in f64 px — **but see the coordinate schema below** |
| `_position.schema` | `canvasOffsetPx` (absolute) or `parentOffsetPx` (relative to parent) |
| `scale.scale` / `relativeScale` | Uniform scale |
| `rotation.rotation` / `relativeRotation` | Degrees |
| `_parent` | `{"index": n}` referencing the objects array, or `null` |
| `size.{width,height}` | Present on sized widgets; frames also carry flat `width`/`height` |
| `style` | **A JSON *string*** of compact keys — see §2.2 |

### ⚠ Coordinates are mostly parent-relative

On the reference board the split is:

| `_position.schema` | Count | Meaning |
|---|---|---|
| `parentOffsetPx` | **485** (81%) | Relative to the parent's **top-left corner** |
| `canvasOffsetPx` | 92 | Absolute canvas position |
| absent (`null`) | 19 | No position — connectors are defined by their endpoints |

**Positions denote widget centres, and the offset is in the parent's own unscaled space.** So resolving a child is:

```
absolute_centre = parent_absolute_centre
                + child_offset × parent_scale
                − parent_size  × parent_scale / 2
```

`parent_scale` is `scale.scale`, which is **absolute** world scale, not `relativeScale` — so each step of the walk uses its own parent's factor rather than a running product. (`relativeScale` is the ratio of the two, and its existence is itself the tell that a parent imposes a scaled coordinate frame on its children.)

The top-left origin was established empirically, not assumed: under that rule **94.4%** of parented widgets land inside their own parent's bounds; reading the offset as centre-relative gives only **18.6%**.

**Both scale factors are load-bearing, and getting them wrong is invisible until you check.** The first implementation omitted them, which is harmless for the 12 frames (all at scale 1) and wrong for everything attached to a scaled widget. Measured against the SVG export — joined widget-by-widget on `initialId`, which the clipboard carries per object and the SVG emits as `data-widget-id`:

| | ignoring parent scale | with parent scale |
|---|---|---|
| Sized widgets within 1px of the SVG | 310 / 313 | **313 / 313** |
| Ink attached to a scaled parent (39 strokes) | median **245px** off, max **641px** | median **0.3px**, max 4px |

The 44 stickies sit a constant 11 local units off in both columns; that is the drop-shadow margin baked into the SVG's `#StickerType1` symbol, and it scales exactly with the sticky's own scale factor, so it is a rendering artifact rather than a placement error.

**Images carry no `size`.** Their extent is `crop.width/height` (falling back to `resource.width/height`), and `_position` is the centre of the *crop* rectangle — verified on the board's 3 genuinely-cropped images, where the SVG's `<g>` translate sits exactly `crop.y × scale` above the displayed top-left because the SVG positions the full source image and clips it. A resolver that reads a missing `size` as zero puts anything drawn on an image half an image away.

Treating everything as absolute puts four-fifths of a board in the wrong place *while still looking superficially plausible* — the most dangerous class of import bug. Nesting composes, so the resolver walks the whole parent chain, with a depth bound so a malformed payload with a parent cycle terminates rather than hanging.

### 2.1 Widget types confirmed

From a full-board copy — **596 objects, all of them mapped**:

| `widgetData.type` | n | Payload |
|---|---|---|
| **`paint`** | 219 | **`points[]` + stroke style — freehand ink** |
| `image` | 122 | `crop{x,y,w,h,shape}`, `resource{id,name,width,height}` |
| `preview` | 91 | `openGraph{title,description,url}`, `path.path`, `visualType`, `resourceWidget` — link cards |
| `text` | 46 | `text` as **rich-text HTML** (`<a href=…>`) |
| `sticker` | 44 | Sticky note; rich-text HTML (`<p>fan</p>`), `ns:author` |
| `embed` | 41 | `custom_data`: title, url, description, iframe html, provider, author; `provider{name}`, `resourceWidget` |

### A link card carries its picture — `resourceWidget`

Miro fetches a page's OpenGraph image **once**, stores it as an ordinary board resource, and
points the widget at it. This is the single most valuable field in the payload, because
re-fetching largely does not work. Probed 2026-07-30 against the board's own links:

| host | links | result today |
|---|---|---|
| alibaba.com | 26 | **200 with no OpenGraph tags** |
| amazon.com | 23 | **404** |
| ebay.com | — | **403** |
| youtube.com | 11 | full title, description, image, icon |
| small sites (independent vendors, …) | most of the rest | fine |

So 49 of 131 links can never be filled in from the live web by a non-browser client, and
**all of them arrive complete in the clipboard.** Note the earlier record here said AliExpress
answers *"too many redirects"*; that is no longer what happens — it completes and serves
nothing. The failure mode moved, the conclusion did not.

```json
"resourceWidget": {
  "id": "3458764500000000004",        // joins the .rtb like any image widget
  "name": "MAH-CR923000P.JPG",
  "meta": {
    "extension": "jpg",
    "externalLink": "https://parts.example.com/public/assets/…/photo.JPG",
    "width": 900, "height": 900
  }
}
```

Two things to know:

- **The shape differs from an image widget's `resource`** — the id sits at the top and the
  dimensions live under `meta`, with no `resource` wrapper. `mapper::resource_widget` decodes
  it separately for that reason.
- **`meta.externalLink` is the origin CDN URL**, and it is fetchable when the page is not:
  `i.ytimg.com/vi/…/hqdefault.jpg` serves to anything. That is the route for a board pasted
  with no `.rtb` beside it.

Present on **64 of 91** previews and **40 of 41** embeds; **57 and 25** of those resolve out of
the reference `.rtb`. Taking them moved `assets_recovered` from 123 to **205** — every asset in
the archive, none now unused. The remainder point at a resource Miro has pruned, and are
deliberately **not** counted as missing assets: a card with no picture is still a whole card.

### `visualType` — which card form the user chose

Undocumented by Miro, so mapped by measurement over the 91 previews:

| `visualType` | n | has `resourceWidget` | median size | → `CardMode` |
|---|---|---|---|---|
| 0 | 30 | 3 | 250 × 289 | `Card` |
| 1 | 18 | 18 | 250 × 203 | `Card` |
| 2 | 43 | 43 | 250 × 361 | `Large` |

Type 2 always carries an image and is much the tallest, so it is the large preview. Type 0 is
the form for a page that offered no picture — only 3 of its 30 have one — and a card asked to
show an image it lacks already falls back to the `Card` layout in the painter, which is what
makes mapping it there safe.

**`CardMode::Link` is never produced from a `preview`.** Miro stores a collapsed link as a
**text widget carrying an `<a href>`**, not as a `preview` at all — which is also why 46 of the
board's `text` items are bare URLs.
| `line` | 18 | Connector — see below |
| `frame` | 12 | `text` is the frame **name**; `prevFrameIndex`, `speakerNotes`, flat `x/y/width/height` |
| `document` | 1 | PDF `resource` + `document.externalLink` |
| `structured_document` | 1 | `content[]` in **Quill delta** form (`{insert, attributes}`) |
| *(group, `type: 10`)* | 1 | `items: [index, …]`, no `widgetData` |

**`paint` is the headline.** Freehand drawings are exposed by **no** Miro REST v2 endpoint and **no** Web SDK method. The clipboard is the only route to them, and at 219 strokes they are the single largest category on this board.

**Connectors (`line`)** are the other high-value type, because their endpoints *bind*:

```json
{ "primary":   { "point": {"x":1,"y":0.5}, "positionType":0, "widgetIndex":219 },
  "secondary": { "point": {"x":0,"y":0.5}, "positionType":0, "widgetIndex":220 },
  "points": [], "_position": null,
  "style": "{\"lc\":3355443,\"ls\":2,\"t\":2,\"lt\":1,\"a_start\":0,\"a_end\":9,\"jump\":0}",
  "line": { "captions": [] } }
```

`widgetIndex` is an index into the objects array, and `point` is a normalised 0–1 attachment on that widget's bounds (`{x:1,y:0.5}` = right edge, vertically centred). Preserving this is what lets a connector re-route when either end is dragged, rather than importing as a dead line.

Still unobserved on this board: `shape`, `card`, `table`, `mindmap_node`, `kanban`. The mapper routes anything unrecognised to `WidgetKind::Unsupported`, preserving raw JSON and naming it in the fidelity report, so an unknown type is reported rather than dropped.

### 2.2 Compact style keys

`style` is a JSON string, e.g. `"{\"fs\":0,\"ffn\":\"Noto Sans\",\"sbc\":16775070}"`. Colours are decimal integers; **`-1` means "none"**, not black.

| Key | Meaning | Confidence |
|---|---|---|
| `sbc` | Sticky background colour | **Verified** — 16775070 = `0xFFF79E`, and the SVG export renders that same sticky `#fff79e` |
| `ffn` | Font family name (`"Noto Sans"`) | Verified — literal string |
| `ta` | Horizontal align: `l` \| `c` \| `r` | Verified — literal values |
| `tc` | Text colour | High — 1710618 = `#1a1a1a` |
| `lc` | Ink stroke colour | High |
| `t` | Ink thickness (px) | High |
| `lo` | Ink opacity (0–1) | High |
| `fs` / `fsa` | Font size / auto-fit flag; `fs:0` + `fsa:1` = auto | High |
| `lh` | Line height, × font size (1.36) | High |
| `tav` | Vertical align (`m`) | High |
| `b` `i` `u` `s` | Bold / italic / underline / strike, 0\|1 | High |
| `brc` `bro` `brw` `brs` `brr` | Border colour / opacity / width / style / radius | Medium |
| `bc` `bo` `bsc` | Background colour / opacity / secondary | Medium |
| `st` `tsc` `fw` `p` `hl` `taw` `tah` `e` | Unidentified | **Unknown** — preserved raw |

Unidentified keys are kept in `Widget::raw` so they can be decoded later without re-exporting anything.

### 2.3 Known limits

1. **References, not bytes.** `image`/`document` widgets carry a `resource.id`, never pixels. Bytes come from the matching `.rtb` (best quality) or the SVG's base64 (downscaled). A board with neither needs an authenticated re-download.
2. **Undocumented and unstable.** Miro can change this whenever they like. Mitigations: version-gate the marker, self-calibrate the byte shift, fail loudly, keep the SVG importer permanently as a fallback.
3. **Capacity unknown.** Whether one Cmd+A/Cmd+C carries a board the size of the reference one is **untested**. If capped, the importer walks frame by frame and stitches on `_parent` + absolute `_position`, which are already absolute so fragments compose without fixups.

---

## 3. SVG export — the correctness oracle

36MB of real vector XML, and critically it **retains Miro's own CSS class names**, so widget types survive. Board extent `41282.89 × 17515.36 px`.

| Signal in the SVG | Count | Meaning |
|---|---|---|
| `<use xlink:href="#StickerType1\|2">` | **44** | Sticky notes, with `fill` giving the exact colour |
| `data-frame="true"` | 12 | Frames |
| `class="shape-element …"` | 47 | **Not shapes** — Miro's rect primitive, used as widget chrome. See below. |
| `class="preview-widget …"` | 91 | Link previews |
| `class="embed-widget"` | 41 | Embeds |
| `#LineHeadArrow…` | 18 | Connector arrowheads |
| `<path d="…">` longer than 500 chars | **134** | Ink strokes / complex geometry |
| `<text>` with content | **429** | Real, readable text (not outlined) |
| `data:image/*;base64` | 122 | Embedded images (105 jpeg, 17 png) |
| `<g transform="translate(x,y) scale(s) rotate(d,cx,cy)">` | 258 | Full affine placement |

Sticky colours recovered: 43 × `#fff79e`, 1 × `#ff9e9e`.

### `shape-element` is not a shape widget — corrected 2026-07-30

This row read "47 | Shapes" and was **wrong**, and it cost real belief: it is the sole
evidence behind the long-standing known defect *"Miro shapes still do not import"*. There
was never anything to import.

Measured against Miro's REST API v2 (`GET /v2/boards/7t3vC4IWfus=/items`), which is a third
independent code path from both the clipboard and the SVG:

- `?type=shape` returns **0 items**, `total: 0`. The filter itself works — `?type=sticky_note`
  correctly returns 44 on the same call. **The board contains zero shape widgets.**
- Walking each of the 47 `shape-element` rects back to its enclosing `data-widget-id` and
  looking that id up in the API: **46 are `text` widgets and 1 is the `doc_format`** widget.
  47 distinct owners, so it is exactly one rect per widget, never two.
- **46 of the 47 are invisible**: `fill="transparent"`, `stroke="transparent"`,
  `stroke-width="0"`, `stroke-opacity="0"`. The 47th is the rich document's white page
  (`fill="#ffffff"`, `rx="8"`, a 10%-opacity black hairline) — chrome, not content.
- The 46 transparent ones each sit inside an `<svg class="shape-background">` wrapper, and
  there are exactly **46** of those — one per text widget.

So `shape-element` is Miro's generic rounded-rect primitive, emitted as the *background* of
text and document widgets. `SvgInventory::shapes` was renamed to `shape_element_rects` and
`miro-peek` now prints it labelled `(widget backgrounds, NOT shape widgets)`, because the
bare number is what misled us.

**Note the oracle never actually compared this field** — `SvgInventory::compare` covers
sticky, frame, link_preview, embed, connector, image and ink, and never shapes. So no test
ever failed; the number was only ever *printed*, and reading it was enough to create a
phantom defect. A count that is displayed but never asserted is a claim nobody checks.

### Full API cross-check of the reference board

The same run reconciles the whole board against the clipboard import, and the clipboard
wins:

| API v2 type | API | clipboard type | clipboard |
|---|---|---|---|
| `paint` | 219 | `ink` | 219 |
| `image` | 122 | `image` | 122 |
| `preview` | 91 | `link_preview` | 91 |
| `text` | 46 | `text` | 46 |
| `sticky_note` | 44 | `sticky` | 44 |
| `embed` | 41 | `embed` | 41 |
| `frame` | 12 | `frame` | 12 |
| `document` | 1 | `document` | 1 |
| `doc_format` | 1 | `rich_document` | 1 |
| `connector` (separate endpoint) | 18 | `connector` | 18 |
| *(not exposed by the API)* | — | `group` | 1 |
| **total** | **595** | | **596** |

Every type matches exactly. The clipboard carries **one more** widget than the API does —
the `group`, which `/v2/boards/{id}/items` does not expose at all. That is the same point
§1 makes about the clipboard carrying widget types no Miro API reaches, now measured from
the other side. **The clipboard importer is not missing board content.**

Connectors are on their own endpoint (`/v2/boards/{id}/connectors`) and are absent from
`/items` — asking only for items and counting 577 would understate the board by 18.

Reproducing this needs a token and a network, so it is deliberately **not** a test. The
board id is derivable offline from the `.rtb`: `board.json`'s `id` is a signed 64-bit
integer, and the API/URL id is base64 of its big-endian bytes —
`-1234567890123456789` → `7t3vC4IWfus=`, confirmed by `GET /v2/boards/7t3vC4IWfus=`
answering `"name": "Reference Board"`.

### The API counts everything and carries barely half of it — do not import through it

The census above is a *type* census, and reading it as an import route would be a mistake.
**54% of the board's items come back with no content payload at all:**

| type | count | has `data` | `isSupported` | verdict |
|---|---|---|---|---|
| `paint` | 219 | **none** | **false** | position only — no path, no points, no geometry, no style |
| `preview` | 91 | **none** | **false** | position only — no URL, no title, no image |
| `image` | 122 | `imageUrl` | true | fetchable with the token |
| `text` | 46 | `content` | true | full HTML + style |
| `sticky_note` | 44 | `content`, `shape` | true | full, with `fillColor` |
| `embed` | 41 | `html`, `description`, `mode`, … | true | full |
| `frame` | 12 | `title`, `format`, `showContent` | true | full |
| `document` | 1 | `documentUrl`, `title` | true | full |
| `doc_format` | 1 | `content`, `contentType`, … | true | full |
| | **577** | | | **310 (54%) content-free** |

Miro marks `paint` and `preview` `"isSupported": false` and returns them as a bare
`{id, type, position, parent}`. So an API-sourced import of this board would lose **every
one of the 219 pen strokes and all 91 link cards** — the two largest categories on it —
while still counting 595 items and looking complete.

This is the same claim §1 makes about the clipboard carrying widget types no Miro API
exposes, now quantified from the other side: **the API exposes 46% of this board's
content.** The clipboard remains the import path, and the `.rtb` remains the asset path.
The API's real job is what it did here — a third independent oracle for *counting*, which
is exactly what caught the phantom shape defect above.

**Two jobs.** (a) An *independent oracle*: a clipboard import of this board must reproduce these counts and colours; any mismatch is a decoder bug, and this is the strongest correctness signal available because the two formats were produced by different Miro code paths. (b) An *image source* for boards with no `.rtb`.

Text is emitted as `<text … textLength lengthAdjust="spacingAndGlyphs">` in Noto Sans — real glyphs with explicit advance widths, which also gives us Miro's own text metrics for wrap-parity checking.

---

## 4. CSV export — link metadata

205 rows of `title,url`, grouped under section headers (e.g. `"ECU"`). Supplementary only; the clipboard's `embed.custom_data` is richer.

---

## 5. Miro REST API v2 — deferred, and weaker than the clipboard

Requires an account and a developer-team app token. Exposes `sticky_note`, `shape`, `text`, `card`, `app_card`, `image`, `document`, `embed`, `frame`, `group`, `connector`, `tag`, plus experimental `mind_map` / `flowchart` / `code`. Rate limits are credit-based: 100,000 credits/min globally, per-call levels of 50 / 100 / 500 / 2000.

It does **not** expose pen drawings, tables, kanban, USM, mockups, or comments — the Web SDK's `board.get()` has the same gaps. So the API returns *less* than the clipboard. Its only advantage is unattended bulk export across many boards, which makes it a later convenience rather than a dependency.

---

## 6. Import strategy

```
clipboard  ──► structure: types, positions, hierarchy, rich text, styles, INK
   .rtb     ──► pixels at original resolution, joined on resource.id
   SVG      ──► correctness oracle + fallback pixels for boards with no .rtb
   CSV      ──► supplementary link metadata
```

Fully offline. No Miro account required.

Every import emits a **fidelity report** listing counts per type, unmapped types by their Miro name, and missing assets. A partial import that looks complete is the failure mode that destroys trust fastest, so the importer is built to over-report rather than under-report.
