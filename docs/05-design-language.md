# Design language

The user supplied the palette and one hard constraint: **"make it beautiful and don't make it look like AI design."** That constraint is the most important line in this document.

---

## 1. Palette

The user's own colourway, whitened once and given a third accent once — both at their
own request, both recorded below rather than overwritten.

| Token | Hex | Was | Role |
|---|---|---|---|
| `bone` | `#FCFDFE` | `#F4F5F6` | Lightest surface — panels, menus, popovers |
| `milk` | `#F7F9FA` | `#EFF1F2` | Default app background |
| `paper` | `#F5F7F8` | — | **The board.** Split from `pearl` — see below |
| `pearl` | `#EBEEF0` | `#E3E6E8` | A raised chrome surface, the step above `bone`'s panels |
| `frost` | `#E5EAED` | `#DDE2E5` | Borders, dividers, inset wells, disabled fills |
| `signal-teal` | `#00A38C` | — | **Primary accent** — selection, active tool, primary buttons |
| `xr-red` | `#E65B58` | | **Destructive only**, now that the teal has the rest |
| `monitor-cyan` | `#6FD6E6` | | Secondary accent — snap guides, hover, informational |

The neutrals are only 4% apart in luminance, which is the point: hierarchy comes from **type, spacing and hairlines**, not from slabs of contrasting grey. Text carries the contrast.

### ⚠ The ramp was translated, not compressed

*"the app is too gray make it more white ish."* Every neutral rose by **exactly 8/255 in
every channel**. The whole ramp sits 3% closer to white; the **three steps between them
are byte-for-byte unchanged**, and so is the `bone`→`frost` distance that draws every
panel edge in the app.

That distinction is the whole of it. This design uses **a hairline where other designs
use a shadow** (§2), so the gaps *are* the structure. Reaching white by squashing them
would have dissolved every panel edge in order to make the app whiter, which is not what
was asked for. Anyone moving these again must move all four together —
`vellum-ui/src/theme.rs`'s own test measures both halves and will say so.

The canvas grid moved with the canvas by the same 8, so the contrast the user tuned in
feedback 11 (27/255) is unchanged. Whitening the board and leaving the grid where it was
would have made the grid *louder*.

### ⚠ Then the canvas split off, because the ramp had run out of room

*"the background color on Miro looks a lot better than the background color in Velm."* Miro's
board is near-white; ours was a distinctly blue-grey `pearl`. A second translation was not
available — `bone` is already `#FCFDFE` and would clip — so this round had to do the thing the
section above forbids, and the way out was to notice that **one token was doing two jobs**.

`pearl` was the board *and* `raised`, a chrome surface that sits on `milk` and has to be
distinguishable from it. Whitening the shared value would have put `raised` within 2/255 of its
own backdrop. So the board took its own token, `paper` `#F5F7F8`, and `pearl` kept the chrome:
the ramp is uncompressed, every panel edge is where it was, and only the board moved.

**How far it can go.** `paper` leaves 7/255 to `bone`, close to Miro's own board-to-card
separation. A card is told apart from the board by its `frost` border — now **16/255** from the
board rather than 6, so *more* legible than before — and by that 7/255 of fill. Past this the
fill difference stops carrying any of it. **If the board still reads grey, the next lever is a
soft shadow under cards, not a whiter canvas.**

**And the grid came down, reversing feedback 11 on purpose.** The delta is **14/12/11**, not
27/24/22. Feedback 11's figure fixed *"the dots are invisible"*; the user has since asked for
Miro's, which are barely there. Both are the user's and this one is later — the test asserts the
new delta and a floor of 8/255, below which they vanish entirely. Do not restore the old number
on the strength of feedback 11 alone.

### The accent is `signal-teal`, and `xr-red` kept only destruction

*"lets come up with another color that wil help the app pop more."* Offered as three
candidates — signal orange, deep teal, cobalt — and the user chose the teal.

**Violet was never among them.** §2's first prohibition is *"No purple"*, which is exactly
where an accent picked for "pop" otherwise lands; `tests/tokens.rs` enforces it whatever
anyone intends.

Splitting the roles was the part that was not asked for and is worth more than the hue:
`xr-red` used to be selection *and* the active tool *and* destructive, so **delete looked
like selected**. It now has one job, and nothing else in the light interface is red.

Two consequences to know before touching it:

- **`on_accent` stays `ink`, not white.** White on `#00A38C` is 3.2:1 and fails AA;
  charcoal is 5.3:1. Same rule the coral was already following, opposite-looking answer.
- **`accent_soft` and `info_soft` can no longer separate by hue.** The teal is 20° from
  `monitor-cyan` where the coral was 160° away, so the two soft washes part by **value**
  instead — 20% and 14% tints rather than 14% and 20%. They meet in the board library,
  where a space row can be selected *and* a drop target at once.

The mark's accented bracket wears the teal too (`assets/logo/mark.svg`), and so does the
app icon `scripts/make-app.sh` draws. The retained dark cut was deliberately **not**
followed — see below.

#### It is a setting now: Preferences ▸ Accent colour

*"in the settings i want you to have an option to the previous red and also blue."* Three
rows — `signal-teal`, `xr-red`, cobalt `#1B62E8` — each with a swatch drawn in the colour it
names, persisted in the library sidecar as a string.

**Three values and not a colour picker**, because each one has to clear 4.5:1 twice: the ink
on the accent, and the ink on the accent's own tint. An arbitrary hue cannot promise that —
a picker offers pale yellow, which fails both, and the interface silently stops being legible
in the two places that say *you are here*. `Palette::with_accent` moves all four tokens
together for the same reason, and derives `on_accent` rather than fixing it: charcoal reads
on the teal and white reads on the cobalt, and a single constant fails AA on one of them.

Choosing `xr-red` restores the arrangement above **in full**, including delete and selected
being one colour. That is what "the previous red" means. The logo does not follow the
setting: the mark's asset and the generated icon are static files.

### Derived text and ink

Not supplied; derived to sit in the same cool family rather than pure black, which would read as harsh against these greys.

| Token | Hex | Role |
|---|---|---|
| `ink` | `#1A1D1F` | Primary text |
| `ink-muted` | `#5C656B` | Secondary text, labels |
| `ink-faint` | `#8B959B` | Placeholders, disabled text |

### ⚠ LIGHT MODE ONLY — dark is not shipped

The user has since said plainly: *"i want only light mode."* That supersedes the earlier
request for a dark mode, and it is the current instruction.

The app is **always light**. No `--dark` flag, no follow-system, no toggle. `appearance.rs`
must not read the OS dark setting — though it must still honour *reduce transparency*.

The dark scale below is **retained but unreachable**. Keeping the tokens costs nothing,
the values are the owner's and would be tedious to recover, and a preference like this
can reverse. Nothing may select them at runtime.

### Dark mode *(retained, not shipped)*

Also the user's own, from a second swatch card in the same system. Use these, not derived values.

| Token | Hex | Role |
|---|---|---|
| `void` | `#0B0D10` | Deepest recess — input wells, inset fields, dropdown backdrops |
| `pearl` | `#1C1F23` | Canvas backdrop; the working field |
| `bone` | `#2B2F34` | Panels, menus, popovers — floating *above* the canvas, so lighter |
| `frost` | `#3A4048` | Borders, dividers *(derived: `bone` lifted toward `A7ACB3`)* |
| `ink` | `#F5F5F6` | Primary text |
| `ink-muted` | `#A7ACB3` | Secondary text, labels |
| `xr-red` | `#C8102E` | Primary accent — selection, active tool |
| `monitor-cyan` | `#57E5FF` | Secondary accent — snap guides, hover, focus |

Two things to preserve when implementing:

1. **The user asked for "not too dark".** `#0B0D10` is near-black and is therefore *not* the background — it is reserved for recessed wells. The app sits on `#1C1F23`, with panels a step lighter at `#2B2F34`. The result reads as charcoal, not void.
2. **Depth inverts between modes.** In light, panels are lighter than the canvas and the canvas recedes. In dark, panels are *also* lighter than the canvas. Panels always float; the canvas always recedes. Do not mirror the ramp mechanically.

The accents shift deliberately across modes rather than being reused: red deepens (`#E65B58` → `#C8102E`) so it holds against dark without glowing, and cyan brightens (`#6FD6E6` → `#57E5FF`) so it stays legible. Same intent, different value — which is exactly why every colour must resolve through a token and no widget may contain a hex literal.

**The dark cut is frozen at the arrangement above and did not follow the light one.** It
still has `xr-red` as its primary accent, doing all three jobs, and it has no teal. That
is deliberate: inventing a dark teal means inventing a colour the user never gave, and
then maintaining it, for a palette nothing can select. If dark is ever revived, that is
the moment to ask them for the value — not now, and not by filtering the light one, which
is the mistake this whole section exists to prevent.

**Only light ships.** See the notice above — this line previously said both modes plus follow-system, which the user has since overridden.

## 2. What "don't make it look like AI design" rules out

Stated as prohibitions, because they are the defaults every generated UI drifts toward:

- **No purple.** No violet-to-blue gradients, no gradient buttons, no gradient text.
- **No *decorative* glassmorphism.** No frosted cards on gradient grounds, no translucency as ornament, no glass stacked on glass. (Translucency as a *material for floating chrome* is a different thing and is explicitly wanted — see §3a.)
- **No pillowy radii.** Corners are `4px`, occasionally `6px`. Not 16, not fully rounded.
- **No shadow on everything.** One hairline border does the work. Shadow only where something genuinely floats above the canvas, and then it is tight and low-opacity.
- **No emoji as iconography.**
- **No centred hero copy** or marketing voice anywhere in the product.
- **No 3D-ish depth** — no bevels, no inner glows, no neumorphism.
- **No oversized whitespace.** This is a dense tool for someone who works in it, not a landing page.

## 3. What it should look like instead

**A precision instrument.** The reference points are technical drawings, machinist tooling, and instrument panels — which is also the kind of work these boards hold.

- **Flat surfaces, hairline separation.** 1px `frost` borders. Panels are distinguished by a single line and a small luminance step, not by shadow.
- **Tight, deliberate spacing** on a 4px grid. Dense enough to hold real controls without feeling cramped.
- **Type carries the hierarchy.** One family, three or four sizes, real weight contrast. Small caps or letterspaced uppercase for section labels — as in the supplied swatch card, where `COLORWAY` is letterspaced caps over hex values in mono.
- **Monospace for anything numeric.** Coordinates, dimensions, zoom percentage, hex values, item counts. Tabular figures so digits do not jitter while dragging.
- **Accent used sparingly.** `xr-red` marks the active tool and the selection; `monitor-cyan` marks snap guides and hover. If more than about 5% of the screen is accented, it is overused.
- **Icons are line drawings**, 1.5px stroke, geometric, consistent optical weight. Drawn as a set, not collected.
- **Motion is functional and fast.** 120–160ms, ease-out. Panels do not bounce. Nothing animates that the user did not cause.

## 3a. Translucent chrome ("liquid glass")

Requested explicitly, after the translucent material in macOS 26 / iOS 26. It needs macOS 26 or later to look the way it is specified here; on earlier versions it degrades to a flat surface.

This is not a reversal of §2. Two different things share the word "glass":

| | Ruled out (§2) | Wanted (here) |
|---|---|---|
| What it is | Decoration | A material for chrome that floats over content |
| Where it goes | Anywhere, especially cards on gradients | Only surfaces genuinely hovering above the canvas |
| Why | Looks current | You can see your own work continuing underneath the toolbar |

For an infinite-canvas app this is more than a style note: a floating toolbar that occludes the board is a real cost, and translucency reduces it.

### Where it applies

**Yes** — left toolbar, the selection context toolbar, the bottom-right zoom cluster, popovers, flyouts, the command palette. Everything that floats above the canvas *and is not a list of words you are reading*.

**No** — the canvas itself, the board library and its rows, the properties panel when docked, dialogs (modal means opaque; a modal you can see through undermines its own job), **menus** — the bar's dropdowns and the right-button menu alike — and anything already sitting on a glass surface. **Glass never stacks.**

> **Menus moved from Yes to No**, and it is worth saying why rather than quietly editing the
> list. They were in the Yes column and the right-button menu was built glass accordingly;
> the user photographed one opened over the reference board with colour bleeding through its
> corner and asked for it to be fixed. The rule that decides it is one paragraph below —
> *legibility wins over the material, every time* — and a menu is the surface with the least
> to gain: it is dense text, on screen for half a second, in front of exactly what you have
> stopped looking at. The bar's own dropdowns were already opaque by accident of egui's
> layout, so this also removes a case where the same command list was translucent from the
> right button and opaque from the menu bar.

### Specification

- **Backdrop**: blur the canvas behind the panel, plus a slight saturation lift so colour beneath stays alive rather than turning grey. A **pure white `#FFFFFF`** tint at **~47%** opacity, adjustable from ~28% to ~90% in Preferences ▸ Transparency. (The dark figure is moot while only light ships.)
  - **The tint is the one pure white in the palette, and the default is under half.** Both on a standing instruction — *"make these like they are liquid glass but increase the transparency range and make the color of them white ffffff"*. The figure has come down twice: 72% as first specified, 63% on *"slighlty more translucent"*, 47% here. Under half is the threshold that matters — past it the board is the louder half of the composite, which is the difference between a panel *lit by* what is behind it and a frosted slab that merely admits it.
  - `bone` (`#FCFDFE`) is still every **opaque** surface. Glass is the only token whose neighbour is the blurred board rather than another neutral, so it is the only one that can go to full value without dissolving a hairline — see `vellum-ui::theme`'s `light::GLASS_WHITE`, which is deliberately its own constant so nobody "tidies" it by moving `BONE`.
- **Edge**: a 1px hairline in `frost`, plus a single specular highlight along the top edge only — a 1px inner line of white. That top-edge catch is what separates the Apple material from a plain blur; it reads as a physical edge picking up light.
  - **The catch is specified by its effect, not by one alpha.** ~14% white is right over the dark cut's panel, where it lifts the top row by 14/255. Over the light cut's it lifted the row by **2/255** and simply was not there: that panel already sits below white, so there are only so many levels of headroom to catch light in. The light cut therefore uses a much higher alpha to land on a comparable lift. Both figures are in `vellum-ui::theme`'s two `glass_highlight` entries with the derivation beside them.
  - **Whitening the ramp took most of what was left, and that is accepted rather than chased.** `bone` is `#FCFDFE` now, which composites at 160/255 over the blurred canvas to ~246 — **9 levels** of headroom where there were 17. The alpha went to 230, the practical ceiling, which recovers ~8/255 of it and cannot do better: a specular catch is a *lift toward white*, and there is barely any white left to lift toward. On this ramp the hairline does more of the separating than the catch does. Do not "fix" this by darkening the panel back down.
  - The line is snapped to the pixel grid before its half-pixel offset. A panel edge landing on a fraction spreads it across two rows at half strength each, and half of very little is nothing.
- **Radius stays 4–6px.** Translucency does not license pillowy corners.
- **Shadow stays tight**: `0 1px 2px` at low opacity, plus a very soft ambient pass. Glass floats; it does not hover in mid-air.
- **Legibility is non-negotiable — but where the line sits is the user's call.** Text on glass must still meet AA against the *worst-case* backdrop, not the average one. Legibility wins over the material, every time, and that is why the slider has a floor at all rather than reaching zero: a control that can make the toolbar wholly invisible has no way back except from memory.
  - What moved on *"increase the transparency range"* is the floor, from ~55% to **~28%** (`Palette::MIN_GLASS_OPACITY`). Say plainly what that trades: ~55% was the point where chrome text still passed over a *busy* board, so the bottom of the new range is comfortable over a plain canvas and can be hard to read over a dense photograph. The default sits at 47%, well above the floor, and one drag puts it back. Reduce Transparency and Increase Contrast still override the whole thing outright.

### Accessibility and fallback

- Honour the OS **Reduce Transparency** setting: fall back to fully opaque `bone`, no blur. Same for high-contrast mode.
- Provide an in-app override too, since a user may want it off on one machine and on elsewhere.
- Detect it at runtime and react live — do not read it once at launch.

### Performance — the binding constraint

The entire project exists because Miro stutters. A blur that costs frame time is not worth having.

- Blur a **downsampled copy** of the already-rendered canvas texture (quarter resolution, dual-pass Kawase or separable Gaussian). Never a full-resolution per-frame blur.
- Only blur the **regions actually behind glass panels**, not the whole frame.
- Cache the blurred result and refresh it only when the canvas beneath actually changes; while a panel is idle over a static board, the cost is zero.
- **Budget: under 0.5ms per frame.** If it exceeds that on the reference board, the material is dropped to a flat tint rather than the frame rate being sacrificed. This is not negotiable and should be asserted by a benchmark.

## 4. Canvas

The canvas is the product; chrome recedes.

- Backdrop `pearl`, subtly darker than the panels, so the working field reads as recessed.
- Grid: dots, crosses or graph lines, in the dedicated `grid` token — **not** `frost`.
  This line used to say `frost`, and it was wrong: against `pearl` that is a 2.4%
  channel delta, 1.05:1, so the dots drew correctly and were invisible. Lines only
  looked like they worked because a full-height bar lays down ~35× the ink per cell at
  the same colour. `grid` is about 10% darker than the canvas — present when looked
  for, quiet when not.
- **The grid's ink is overridable, and the override is app-wide.** View ▸ Grid offers a
  named colour list and an opacity slider; both write one `Color` in the library sidecar,
  because a colour and its alpha describe the same pixel. `None` — which is every board
  today — keeps following the `grid` token, so a future move of the token still reaches
  every board nobody has restyled. The **pattern** is app-wide too, falling back to the
  board's own only until a global choice is made; the **background colour** is per board,
  and that asymmetry is deliberate. A board's colour is what tells two boards apart; its
  grid is a drawing aid that wants to be the same everywhere.
- Selection: 1px `xr-red` outline, square handles, no glow.
- Snap guides: 1px `monitor-cyan`, and they vanish the instant the drag ends.
- Floating panels over the canvas — matching Miro's ergonomics, per `04-ui-reference.md` — but flat, hairline-bordered, tightly radiused.

## 4a. The mark

**Viewport** — four corner brackets around a frame that is never drawn. Assets and usage rules in [`assets/logo/`](../assets/logo/).

Chosen over four alternatives (a datum crosshair, an overflowing frame, a V monogram, wordmark-only) for one reason: **its meaning comes out of the product rather than being applied to it.** The mark is the same shape as the canvas selection indicator, so it is something the user sees thousands of times a day without it reading as branding. Nothing else on the shortlist had that.

It also survives the tests a mark has to pass — legible at 16px, works in one colour, needs no gradient, container or illustration. The single accented corner gives it a reading direction instead of anonymous four-fold symmetry.

The red differs by mode (`#E65B58` light, `#C8102E` dark) and ships as two files rather than one filtered file, because those are two specifications, not one colour transformed.

## 5. Typography

- **UI:** the platform sans (SF Pro on macOS, Segoe UI Variable on Windows). Native beats a bundled webfont for legibility and for looking like it belongs on the machine.
- **Numeric / mono:** SF Mono, Cascadia Mono.
- **Canvas default:** Inter, **bundled** — `assets/fonts/Inter-{Regular,Bold}.ttf`, compiled in
  via `vellum_text::BUNDLED_FONTS`. Miro's boards use Noto Sans, so `vellum-text` still loads the
  machine's fonts too and resolves Noto for import fidelity; what the bundle changes is only the
  *default*.
  - **It is bundled as of feedback 27, and for years it was not — this line was aspirational.**
    The history is worth keeping, because the bug it caused is subtle: `TextEngine::new` took
    `FontSystem::new`, a `font_family: None` request resolved through fontdb's `sans-serif`
    alias, and on the development machine that landed on **Noto Sans** — installed for import
    fidelity and shipping *regular weight only*.
  - **Loading the faces is not what fixed it.** `set_sans_serif_family("Inter")` is: the alias is
    what `family_has_bold(None)` interrogates, so without that one line the bundle would sit in
    the database unreferenced and every bold span would still shape at regular weight. A/B'd.
  - **Why bundle at all, beyond bold.** A board is a document that has to lay out the same
    tomorrow. Metrics decide wrapping, auto-fit sizes and — for a mind map — the *geometry of the
    tree*, so a board shaped against whatever the host happens to have would re-flow on another
    machine and would not match its own exported SVG.
  - Static Regular and Bold rather than the variable file: fontdb matches weights across *faces*,
    and a variable font presents one. The OFL notice ships in the app bundle.
  - **That had a visible consequence, now guarded.** Asking cosmic-text for a weight
    the resolved family has not got does not fall back to that family's regular face —
    it leaves the family, and here it landed on a monospace one. Measured at 18px, the
    advance ratio of `WWWWWWWW` to `IIIIIIII` was 4.11 regular and **1.00** "bold":
    every bold span on the canvas was being set in Courier, and the box measured
    around it was wrong to match. `TextEngine::family_has_bold` now drops the weight
    request instead, so a bold span whose family has no bold face is set **regular in
    the right family** — emphasis lost, typeface kept, which is the lesser
    degradation of the two available. cosmic-text has no synthetic bold.
  - Bundling Inter would fix the emphasis as well as the typeface, and is the right
    answer; it is a licence check and a binary in the repo rather than a code change,
    so it is recorded here rather than done.

Sizes: `11px` labels · `13px` body and controls · `15px` panel titles · `20px` screen titles. Line height 1.4 for UI, 1.36 on canvas text to match Miro's default.

## 6. Accessibility

Text meets WCAG AA against its own surface. The accents are decorative and are never the sole carrier of meaning — selection also changes handle geometry, snap guides also change cursor. Full keyboard navigation, visible focus rings in `monitor-cyan`, and a high-contrast mode that deepens `ink` and thickens borders.
