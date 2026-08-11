# UI reference — Miro's actual layout

Transcribed from screenshots of a real Miro account (desktop app, July 2026), kept
locally in `Screenshots/` — git-ignored, because they show real board names.

**Purpose: muscle memory.** Velm's users arrive with years of habit in Miro. Controls should sit where their hands already reach, and keyboard shortcuts should match. This is about *behaviour and placement*, not appearance — do not copy Miro's icons, wordmark, or colour identity. Velm should look like its own product that happens to work the way you expect.

Where Miro is *bad*, we are explicitly allowed to be better. Noted inline as **↑improve**.

---

## 1. Board view

```
┌──────────────────────────────────────────────────────────────────────┐
│ ☰  [logo]  ✎ Board name  ⋮          [plan] [reactions][chat][video]  │
│                                      [avatars] [▶ present] [Share]   │
├──────────────────────────────────────────────────────────────────────┤
│ ⬤ AI                                                                 │
│ ┌──┐                                                                 │
│ │▶ │ select        V                                                 │
│ │⊞ │ templates ▸                                                     │
│ │▢ │ sticky        N              CANVAS                             │
│ │T │ text          T                                                 │
│ │⊟ │ frame         F                                                 │
│ │◇ │ shapes ▸      S                                                 │
│ │✎ │ pen           P                                                 │
│ │💬│ comment       C                                                 │
│ │⌗ │ frame                                                           │
│ │⬆ │ upload                                                          │
│ │◈ │ diagram shapes ▸                                                │
│ │⁂ │ mind map ▸                                                      │
│ │+ │ more                                                            │
│ └──┘                                                                 │
│ ┌──┐                                                                 │
│ │↶ │ undo                                                            │
│ │↷ │ redo                          ┌────────────────────────────┐    │
│ └──┘                               │ ⊞  −  109%  +  ?           │    │
└────────────────────────────────────┴────────────────────────────┴────┘
```

**Left toolbar** — a single floating rounded column, vertically centred, ~44px wide, with the AI button detached above and undo/redo detached below. Active tool is highlighted with a tinted background. Every entry has a tooltip on hover showing name + shortcut.

**Bottom-right cluster** — frames/minimap toggle, zoom out, zoom percentage, zoom in, help. One rounded pill.

**Top bar** — floating rounded panels, not a full-width bar. Left panel holds menu + board identity; right panel holds session/sharing controls.

**↑improve:** Miro's toolbar has 14 entries with no grouping, several of which (templates, diagram shapes, mind map) open near-identical pickers. Group ours: *create* (sticky, text, shape, frame), *draw* (pen, eraser), *connect* (connector), *insert* (image, upload). Collaboration entries are omitted entirely — the user does not use them, so they are not built. No dead buttons.

## 2. Toolbar flyouts

**Shapes (S)** — a compact list, not a grid:

| Entry | Key |
|---|---|
| Line | `L` |
| Arrow | |
| Elbow arrow | |
| Block arrow | |
| Rectangle | `R` |
| Oval | `O` |
| Rhombus | |
| Triangle | |
| Divider | |
| More shapes → *(opens the full picker)* | |
| Diagram | |

The last-used shape becomes the toolbar button's icon. Worth copying — it makes repeat use one click.

**Templates ▸** — Prototype, Diagram, Table, Timeline, Kanban, Doc, Slides, Engage activities, Talktrack, Flows.

## 3. Full shape picker (left panel, ~320px)

```
Diagramming shapes                        [⬇] [✕]
[ Search shapes                              ]
▸ My Shapes            [Browse and upload SVG shapes]
▾ Basic Shapes         🎨 Apply colors ▾
   ▢ ▢ ○ △ ◇ 💬 ▱ ☆ ➡ ⬅ ↔ ⬠ ⬡ ⬢ ▥ ⏢ ✳ ✚ ⛁ { } ⌦ ⌫
▾ Flowchart            🎨 Apply colors ▾
   (20 forms)
▸ Callouts
▾ AWS                  (icon grid)
                       [ Manage shapes ]
                       [ + Create diagram ]
```

`vellum-shapes` already implements the Basic and Flowchart sets. **Search, categories, and "Apply colors" are the parts to replicate.** Custom SVG upload is a real feature worth having. AWS icon packs are out of scope.

## 4. Main menu (⋮ next to the board name)

Top level: **Board · Edit · View · Preferences · Accessibility** — plus Assign status and Send to interactive display, both of which we skip.

**Board →** Catch up · New board · Duplicate · **Export ▸** · Move to · Star this board · Delete · Background color ▸ · Start view · Lock default view · History · Details

**Export ▸** Save as PDF · Export as image · Save board as template · Export to spreadsheet (CSV) · Download board backup · Embed · Save to Google Drive · Share as presentation

**Edit →** Undo `⌘Z` · Redo `⌘Y` · Commands `⌘K` · Find `⌘F`

Notes for our build:
- **Redo is `⌘Y` in Miro.** Bind **both** `⌘Y` and `⌘⇧Z`; the latter is the macOS convention and Miro's choice is the odd one out.
- `⌘K` command palette and `⌘F` find are confirmed — both already planned.
- Cloud entries (Google Drive, Embed, Catch up, Share as presentation) are dropped. Our Export gains **PNG / PDF / SVG / CSV** and a **`.velm` backup**.
- "Background color" and "Start view" (the default camera position a board opens at) are cheap and genuinely useful — include both. **Both are built.** Background is a submenu rather than a single command: a colour *and* a pattern, because "the green board with dots" and "the green board with lines" are the same board — `vellum_doc::Background`, on the document, so it travels with the board rather than with the machine.
- "History" maps onto our Loro version history.

## 5. Board library / start screen

```
┌─────────────┬──────────────────────────────────────────────────┐
│ AB Account ▾│  [logo]              [Upgrade] [Invite] [🔔] [👤] │
│ 🔍 Search   │                                                  │
│ ⌂ Home      │        How do you want to start?                 │
│ 🕐 Recent   │        [ Search or ask anything      ⌘⇧E ]        │
│ ☆ Starred   │                                                  │
│ ▶ Recordings│   Templates for All roles ▾                      │
│ ─────────── │   [+ Blank] [tmpl] [tmpl] [tmpl] …               │
│ Spaces    + │                                                  │
│  Personal 📌│   Boards in this team      [Explore templates]   │
│  Research 📌│                            [+ Create new]        │
│  Design   📌│   Filter by ▾  Sort by ▾            [▦] [☰]      │
│  Archive    │   ─────────────────────────────────────────────  │
│  Planning   │   Name           Space    Last opened   Owner  ☆ │
│  Posters    │   🖊 Notes        —        Today         AB    ⋮ │
│             │   📐 Site plan   Design   Today         AB    ⋮ │
└─────────────┴──────────────────────────────────────────────────┘
```

Structure to replicate:

- **Left sidebar**: search, Home / Recent / Starred, then **Spaces** — a real account has half a dozen of them, several pinned. Spaces are a first-class organising concept, not a nice-to-have. Hovering a Space reveals ⋮, +, and pin.
- **Board rows**: per-board emoji/icon, name, "Modified by … , <date>", Space, Last opened, star, ⋮ menu.
- **Grid ▦ / list ☰ toggle**, Filter by, Sort by.
- **Template gallery** with a "Blank board" tile first.

**↑improve:** Miro's start screen leads with a search box and template marketing. The user's actual intent is almost always "open the board I was just in". Lead with **Recent boards**; keep templates present but secondary.

**Import from Miro** belongs here as a prominent entry with plain instructions — the user found the copy/paste flow confusing when described in prose:

> **1.** Open the board in Miro **2.** Press `⌘A` then `⌘C` **3.** Come back here and press `⌘V`

## 6. Interaction conventions to match

| Action | Miro | Notes |
|---|---|---|
| Pan | Two-finger scroll, space+drag, middle-drag | **Not** left-drag — that is marquee select |
| Zoom | Pinch, `⌘`/`ctrl` + scroll | Anchored at the cursor |
| Marquee select | Left-drag on empty canvas | |
| Zoom level | Shown as a percentage, clickable | 109% in the screenshots — arbitrary values, not fixed steps |
| Undo/redo | Toolbar buttons *and* `⌘Z`/`⌘Y` | Redo greys out when unavailable |
| Tooltips | Name + shortcut on every tool | |

## 6a. The selection's own controls — the toolbar above it, and the right button

Transcribed from four screenshots of Miro driving a real board. **This is where a board
is actually driven from**; §4's `⋮` menu is for the *board*, not for what is on it.

### The floating toolbar

A single horizontal strip in a white rounded panel, sitting **directly above the
selection** and centred on it, appearing with the selection and leaving with it. Its
contents change with what is selected:

| Selected | Miro's strip, left to right |
|---|---|
| Image | `image.png` · Convert to · flip · download · ALT · crop · pen · mask · spinner · comment · lock · AI · `⋮` |
| Link card | favicon + title · display mode · open ↗ · expand · `⋮` |
| Video embed | favicon + title · reload · display mode · open ↗ · expand · `⋮` |
| Pen stroke | **width slider · colour · lock · `⋮`** — four, and no more |

The pattern that matters is the last row. A pen stroke gets *only* what a stroke has.
Miro does not pad the strip out with controls that do not apply, and `⋮` is always the
last button.

### The right-click menu

Opens at the pointer. Ordered clipboard verbs first, then whatever is specific to the
thing clicked, then structure, then the rest:

```
Start a Flow                         (image only)
─────
Copy                    ⌘C
Copy link               ⌘⌥⇧C
Copy as image           ⌘⇧C
Duplicate               ⌘D
Delete                  Delete
─────
Add comment
Scale to original size               (image only)
Add to Saved files                   (image only)
Download                             (image only)
Set as default view                  (cards and embeds only)
─────
Arrange                 ▸
Link to                 ⌥⌘K
Lock                    ⌘⇧L
Create frame
─────
Save as template
Embed this widget                    (cards and embeds only)
Info                    ▸
```

**What Velm implements**, and what it deliberately does not: the clipboard band,
Arrange ▸, Lock and the card rows are all there. *Add comment* is out with the rest of
collaboration (`docs/features/README.md` §11). *Copy as image*, *Save as template*,
*Start a Flow*, *Link to* and *Create frame* are not built; nothing inert stands in for
them, so they are simply absent rather than greyed out. Velm's canvas menu — which Miro
also has — carries Paste, Select all, the two zoom fits, Minimap, Background ▸, Grid ▸ and
Set start view.

## 6b. The native menu bar (the desktop app's, not the web app's)

Miro's desktop build is an Electron shell, so its macOS menu bar is largely the shell's
own — which is why *Refresh current tab*, *Force Reload*, *Show spell check*, *Paste and
Match Style* and *AutoFill* appear in it. Transcribed from the user's screenshots, with
what Velm does about each:

| Miro | Velm |
|---|---|
| **File** — Close tab ⌘W · Select next/previous tab · Open recently closed board · Copy board link | New board · New tab · Open… · Import from Miro… · Save · Save as… · Export ▸ · Close board ⌘W · Select next/previous tab ⌘⌥→/← |
| **Edit** — Cut · Copy · Paste · Paste and Match Style · Select All · Emoji & Symbols · AutoFill | Undo · Redo · Cut · Copy · Paste · Duplicate · Delete · Select all · Arrange ▸ · Find… · Commands… — and macOS adds Emoji & Symbols and AutoFill itself |
| **View** — Refresh current tab · Force Reload · Actual Size · Zoom In/Out · Toggle Full Screen · Show spell check | Zoom in/out · Zoom to fit · Zoom to selection · Zoom to 100% · Snap to grid · Minimap · Properties panel · start view · Presentation mode · Toggle Full Screen — the in-app View ▸ **Grid ▸** submenu has no native equivalent (a list of swatches and a slider is not a menu row), so the toggle is what the bar carries |
| **Window** — Minimize · Zoom · Fill · Center · Move & Resize ▸ · Full Screen Tile ▸ · Bring All to Front | The same, and **not written by us**: it is AppKit's, handed the submenu with `set_as_windows_menu_for_nsapp` |

The two reload rows have no meaning without a webview, and *Copy board link* and *Open
recently closed board* need a service Velm has none of — so they are absent rather than
inert, per §1.

**The shortcut column is deliberately empty for Cut, Copy, Paste, Select all, Undo and
Redo.** A native key equivalent is claimed before the key reaches the window, and in Velm
those six belong to whatever has the keyboard — the on-canvas caret or an `egui` text
field. See `vellum_app::menubar` for the whole argument.

## 7. Visual direction

Miro's chrome is light grey (`#f5f5f5`-ish canvas), white floating panels, ~12px corner radii, soft shadows, a purple accent, and generous padding. That layout language is worth keeping — floating rounded panels over the canvas read well and keep the canvas dominant.

Ours should be visually distinct: **its own accent colour, its own icon set, and a proper dark mode** (Miro's is weak). Match the *ergonomics*, not the skin.
