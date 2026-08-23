// The editing chrome for the browser client: a tool palette, a right-click menu, and the
// small toolbar that floats above a selection.
//
// `web/chrome.js` is the model for everything in here — the CSS tokens, the 44px targets,
// the `pointer-events: none` container with one rule taking it back, the `mountX(mod, …)`
// shape and the idempotent unmount. Read that file first; this one deliberately repeats its
// idioms rather than inventing a second house style two files into a port.
//
// # ⚠ Why this is HTML and not `vellum-ui`
//
// The desktop's chrome is 27,000 lines of `egui`, and `egui-winit` does not build for
// wasm32 — `docs/08-web.md` §2 and `CLAUDE.md`'s web entries both record it. So the *shape*
// of the interface is ported and none of the code is: `crate::toolbar`'s groups,
// `crate::context_menu::rows` and `crate::context_bar::controls` are three pure functions
// answering "what is on this surface", and their three equivalents here — `TOOL_GROUPS`,
// `menuRows` and `barControls` — are pure in exactly the same way, for exactly the same
// reason. Which rows a selection gets is decided in one place and can be driven by a DOM
// shim with no browser, no GPU and no wasm.
//
// # ⚠ Derived from what the selection *has*, never from its kind
//
// `context_bar.rs`'s own header is the rule and it is worth restating because it is the part
// that is easy to lose in a port: *an ink stroke gets a width, a colour, a lock and a `⋮` —
// not because a `match` on the kind says so, but because ink has a stroke and no fill and no
// words.* `barControls` reads `selection_summary()`'s fields and nothing else, so an item
// kind nobody has thought of yet arrives with a correct bar instead of a blank one.
//
// # ⚠ Nothing here is inert, and nothing here is silently absent
//
// Every control tests `typeof mod.fn === 'function'` before it is wired. A control whose
// export the build has not got is drawn **disabled with a tooltip naming the missing
// function**, which is `CLAUDE.md`'s house rule and the opposite of what `chrome.js` does
// with its zoom cluster: that file omits a *viewer* control with nothing behind it, because
// a reader who never sees a zoom button is not owed an explanation. This is an *editor*, the
// user asked for it, and "where is Delete" deserves an answer. So the honest split is:
//
//   - a control missing its **verb**  → drawn, disabled, tooltip names the export;
//   - a surface missing its **input** → not drawn at all, because there is nothing to
//     derive from and nowhere to put it. That is only the context bar, which cannot know
//     what to show or where to float without `selection_summary()`.
//
// A build with none of the editing exports draws a full palette and a full menu with every
// row disabled. That is the intended state while the Rust half is being written, not a
// broken one.
//
// # ⚠ The wire, measured rather than assumed
//
// `crates/vellum-web/src/style.rs` is the authority for everything this file sends and reads,
// and it was read rather than guessed — which mattered, because two of the shapes this file
// was originally specified with would have been **rejected**. A colour is `{hex, alpha}` with
// six digits and a separate byte, and an eight-digit `#rrggbbaa` is refused on the way in by
// design. A field is `{state}` with three states — `uniform`, `unset`, `mixed` — and *absent*
// is the key being missing altogether, so a two-state reader silently turns "this shape has no
// fill" into "nothing here has a fill". The padlock is `{"locked":true}` through
// `style_selection`, and the reordering verbs are bare JSON strings through
// `transform_selection`. `bounds` is a **world-space** box and has to be put through the camera
// and the device pixel ratio before it can place anything.
//
// # ⚠ The names are resolved, not assumed
//
// The Rust half of this feature is being written in parallel, and its exports landed as
// `velm_undo` / `velm_delete_selection` / `velm_selection_count` where the contract this file
// was written to said `undo` and `delete_selection`. `EXPORTS` is the one table that knows
// both spellings, so neither half had to wait for the other and neither guessed. Anything
// unmatched is still named in a tooltip by its **canonical** name, so the reader is told which
// function to write rather than which of two spellings was tried.
//
// # ⚠ Icons are drawn, never typed
//
// Line drawings on a 20 grid with square caps, as `chrome.js` draws its own. `CLAUDE.md`
// trap 10: a character outside the bundled face draws tofu, and the desktop app has already
// paid for this three times — `↗`, `▶` and `ⓘ` are all geometry there for the same reason.
// No emoji, no `⋮`, no `✎`.
//
// # ⚠ The two collisions with `chrome.js`, both handled here
//
//   - **Escape.** `chrome.js` binds it to *leave the board*. Closing a menu with Escape must
//     not navigate the reader off the page, so this file listens in the **capture** phase and
//     calls `stopPropagation` **only while a menu or a popover is open**. Every other Escape
//     still reaches `chrome.js` untouched.
//   - **The bar and the bar.** `chrome.js` sits top-left and the palette sits left-centre.
//     The floating selection bar is clamped clear of both, measured from their live
//     `getBoundingClientRect()` rather than from constants — a floating toolbar underneath
//     the tool palette is a row of controls that cannot be clicked, and that shipped once on
//     the desktop.

const STYLE_ID = 'velm-tools-style';

/// The gap between a floating surface and whatever it is keeping clear of.
const GAP = 8;
/// How far a finger may travel before a long press stops being a long press.
const PRESS_SLOP = 8;
/// How long a finger has to rest before it means "right-click". Matches the platform
/// convention on both mobile browsers; shorter reads as an accidental menu on every tap
/// that lingers, longer and nobody discovers it exists.
const PRESS_MS = 500;

/// The surfaces currently on the page, so a second `mountTools` can take the first down.
///
/// ⚠ Idempotent for the same reason `chrome.js` is, and the stakes are higher here: this
/// file installs three window-level listeners and a rAF poll, so a second mount without
/// this leaves the first one's keydown handler arming a tool nobody asked for and the first
/// one's poll writing into elements that have left the document.
let mounted = null;

// ---------------------------------------------------------------------------------------
// The tools
// ---------------------------------------------------------------------------------------

/// The palette, grouped as the desktop's `toolbar::GROUPS` groups it: navigate · create ·
/// draw · connect · more.
///
/// ⚠ **Connector is on the palette here and is behind *More* on the desktop.** That is a
/// deliberate divergence and not a mistake to tidy up: the desktop folded six tools away on
/// the user's own *"i just dont use those enough"*, about a palette they use with a mouse
/// every day. Nothing here has heard that about the browser build, and a connector is the
/// one tool of the six that draws a relationship rather than an object. The desktop's
/// grouping is otherwise reproduced exactly, including *Frame* belonging with **create**
/// rather than with the imports.
///
/// The `key` is the desktop's own single-letter binding (`vellum_ui::Tool::shortcut`) and it
/// is bound below — the tooltip's hint is derived from this field rather than written twice,
/// which is the rule `Tool::shortcut_hint` exists to enforce there. A tool with no key shows
/// no hint: a hint for a key nobody bound is a described gesture the user cannot perform.
const TOOL_GROUPS = [
  [
    { id: 'select', label: 'Select', key: 'v' },
    { id: 'hand', label: 'Hand', key: 'h' },
  ],
  [
    { id: 'sticky', label: 'Sticky note', key: 'n' },
    { id: 'text', label: 'Text', key: 't' },
    { id: 'shape', label: 'Shape', key: 's' },
    { id: 'frame', label: 'Frame', key: 'f' },
  ],
  [
    { id: 'pen', label: 'Pen', key: 'p' },
    { id: 'eraser', label: 'Eraser', key: 'e' },
  ],
  [
    { id: 'connector', label: 'Connector', key: 'c' },
  ],
];

/// What **More** folds away — the desktop's `Tool::OCCASIONAL`, less the connector.
///
/// None of these has a single-key binding on the desktop either, which is the same
/// judgement said twice: a tool nobody places daily does not earn a scarce letter.
const MORE_TOOLS = [
  { id: 'table', label: 'Table' },
  { id: 'chart', label: 'Chart' },
  { id: 'kanban', label: 'Kanban board' },
  { id: 'mindmap', label: 'Mind map' },
  { id: 'image', label: 'Image' },
];

// ---------------------------------------------------------------------------------------
// The menu
// ---------------------------------------------------------------------------------------

/// Every row, once.
///
/// ⚠ **One table, two lists.** `context_menu.rs` builds both of its lists from one `Row`
/// enum so a verb's label and its reason for being disabled cannot differ between the
/// canvas menu and the selection menu — *"a context menu built from its own literals is a
/// second copy of the command table, and the copy is what goes stale."* The lists below
/// name keys of this object and never spell a label of their own.
///
/// `fn` is the wasm export the row runs and `arg` is what it is called with. A row whose
/// `fn` the module has not got is drawn disabled with `fn` in its tooltip, which is how a
/// half-built wasm side stays honest without this file needing to know which half.
const ROWS = {
  cut: { label: 'Cut', via: [['cut_selection']] },
  copy: { label: 'Copy', via: [['copy_selection']] },
  paste: { label: 'Paste', via: [['paste']] },
  // ⚠ The only two rows whose **answer** is the point. Every other verb here acts on the
  // board and returns nothing worth keeping; these two return the file. `download` is what
  // tells `runRow` to hand it over rather than drop it on the floor — which is what a row
  // written in the ordinary shape would silently have done.
  exportSvg: {
    label: 'Download as SVG',
    via: [['export_board', () => 'svg']],
    download: { name: 'board.svg', media: 'image/svg+xml', warnsAboutPictures: true },
  },
  exportCsv: {
    label: 'Download as spreadsheet',
    via: [['export_board', () => 'csv']],
    download: { name: 'board.csv', media: 'text/csv;charset=utf-8' },
  },
  duplicate: { label: 'Duplicate', via: [['duplicate_selection']] },
  delete: { label: 'Delete', via: [['delete_selection']], danger: true },
  // ⚠ Two routes, in the order the crate on disk actually answers. `style.rs` takes the four
  // reordering verbs as externally-tagged `Transform` — a bare JSON string — and only falls
  // back to the flat export the original contract named. Neither is guessed: the first is
  // measured off `crates/vellum-web/src/style.rs`, the second is what this file was asked to
  // assume, and a build with either one works.
  front: {
    label: 'Bring to front',
    via: [['transform_selection', () => JSON.stringify('bring_to_front')], ['bring_to_front']],
  },
  back: {
    label: 'Send to back',
    via: [['transform_selection', () => JSON.stringify('send_to_back')], ['send_to_back']],
  },
  // The padlock is a `StyleEdit` on the wire — `{"locked":true}` — and it is the one style
  // edit `apply_style` lets reach a locked item, because otherwise locking is a one-way door.
  lock: {
    label: 'Lock',
    via: [['style_selection', () => JSON.stringify({ locked: true })], ['lock_selection', () => true]],
  },
  unlock: {
    label: 'Unlock',
    via: [['style_selection', () => JSON.stringify({ locked: false })], ['lock_selection', () => false]],
  },
  selectAll: { label: 'Select all', via: [['select_all']] },
};

/// Which rows a target gets. Pure, and the single definition — the DOM equivalent of
/// `context_menu::rows`.
///
/// `summary` is `selection_summary()`'s answer, or `null` when the build cannot report one.
/// The order is Miro's, transcribed in `docs/04-ui-reference.md` §6a: the clipboard verbs
/// first because that is what a right-click is usually for, then structure, then the rest.
///
/// ⚠ **Lock and Unlock are one row when the answer is one word.** `context_menu.rs` shows
/// *Lock* on an unlocked selection and *Unlock* on a locked one, because offering the pair
/// means one of them is always greyed out — noise, on a menu this short. A **mixed**
/// selection is the exception it names: neither label is the whole truth there, so both
/// rows are offered.
export function menuRows(target, summary) {
  if (target === 'canvas') {
    // ⚠ Export is a **canvas** verb, not a selection one, and the asymmetry is deliberate:
    // both formats emit `Scope::Board`, so offering them over a selection would promise a
    // partial export this door does not make. The desktop reaches selection-scoped export
    // through a menu that can say which it is doing; this one cannot, so it does one thing.
    return ['paste', 'sep', 'exportSvg', 'exportCsv', 'sep', 'selectAll'];
  }
  const out = ['cut', 'copy', 'duplicate', 'delete', 'sep', 'front', 'back', 'sep'];
  const locked = summary && summary.locked ? summary.locked : { state: 'absent' };
  if (locked.state === 'mixed') {
    out.push('lock', 'unlock');
  } else if (locked.state === 'uniform' && locked.value === true) {
    out.push('unlock');
  } else {
    out.push('lock');
  }
  out.push('sep', 'selectAll');
  return out;
}

// ---------------------------------------------------------------------------------------
// The context bar
// ---------------------------------------------------------------------------------------

/// Which controls float above the selection.
///
/// ⚠ This is `context_bar::controls` ported rule for rule, and the rules are the whole
/// value of it. Read `crates/vellum-ui/src/context_bar.rs` before changing anything here.
///
///   - **Derived from the properties the selection has**, never from its kind. Each field
///     is `absent` for everything that has not got it, which is what makes `!absent` the
///     honest test.
///   - **Opacity is offered when the fill is not the whole item.** A sticky *is* its colour,
///     so its swatch's alpha says everything and a second control beside it would be two
///     ways to say one thing. A shape paints an interior *and* an outline *and* its words,
///     and no single swatch fades all three — the user went looking for exactly this
///     (*"where is the rtransparencuy slider"*), so it is derived from what the selection
///     paints rather than from the kind.
///   - **Lock and `⋮` are pinned to the right end**, so their position never moves. A lock
///     button that slides horizontally with the selection's kind is one you have to look
///     for every time.
///
/// `summary` is `selection_summary()`'s answer. Returns `[]` for an empty selection, which
/// is what takes the bar off the screen.
export function barControls(summary) {
  const out = [];
  if (!summary || !summary.count) return out;
  const has = (name) => Boolean(summary[name]) && summary[name].state !== 'absent';

  if (has('fill')) out.push('fill');
  if (has('stroke')) {
    out.push('stroke');
    out.push('strokeWidth');
  }
  // The clause that carries the argument above: a fill suppresses opacity **only when the
  // fill is the whole item**.
  //
  // ⚠ **`style.rs`'s summary already applies this rule**, and omits the field entirely for a
  // sticky — its own comment quotes the same sentence. So this is deliberately the *same*
  // predicate said twice rather than a second opinion: both are literally "has a fill and no
  // stroke", and there is no input on which they can answer differently. It is kept because a
  // build whose summary reports opacity unconditionally — an older wasm, or the flatter shape
  // this file was first specified against — would otherwise put two alphas on a sticky's bar,
  // which is the exact defect a review removed from the desktop once. **If the two are ever
  // collapsed, `style.rs` is the one to keep**: it is upstream, and it is where the desktop's
  // own derivation was ported.
  const fillIsTheWholeItem = has('fill') && !has('stroke');
  if (has('opacity') && !fillIsTheWholeItem) out.push('opacity');

  if (out.length) out.push('sep');
  out.push('lock');
  out.push('more');
  return out;
}

/// The board's swatch grid — `vellum_ui::color::SWATCHES`, byte for byte.
///
/// Copied rather than derived, because the wasm module does not export it and a second
/// palette invented here would be a third place the product's colours live. Row 1 is Miro's
/// sticky colours, row 2 the neutrals including `paper` — the board's own colour, so "the
/// same as the background" really is — and row 3 the accents. No violet: `docs/05` §2 rules
/// purple out, and row 1's is Miro's rather than a choice.
const SWATCHES = [
  ['#FFF79E', '#FFCE8A', '#FF9E9E', '#F5A3D8', '#D1A8F5', '#A6C6FF', '#9EE5F5', '#A8E6B8'],
  ['#FFFFFF', '#FCFDFE', '#F2F2F2', '#E5EAED', '#B4BDC2', '#8B959B', '#5C656B', '#1A1D1F'],
  ['#E65B58', '#E87B1F', '#E3B50B', '#1F7A55', '#00A38C', '#6FD6E6', '#376FF5', '#C23A8E'],
];

// ---------------------------------------------------------------------------------------
// Icons — line drawings on a 20 grid, 1.5px stroke, square caps.
// ---------------------------------------------------------------------------------------
//
// The same construction `chrome.js` uses and the same construction `assets/logo/mark.svg`
// uses, so the two bars on screen at once are drawn by one hand. None is accented: in Velm
// the accent means *selection* or *the active tool*, and the palette says which tool is
// active with a tinted background rather than by recolouring a glyph.

const ICONS = {
  // A pointer. Closed, so the outline reads as an arrow rather than as a stray tick.
  select: 'M5 3.5l10.5 6.5-4.7 1.3-1.3 4.7z',
  // A hand, palm and three fingers. Kept blunt on purpose — at 20px a realistic hand is mud.
  hand: 'M7 12V5.5h1.8V10m0-1.4h1.8V10m0-1.2h1.8v1.4m0-.6h1.7v4.3a3.2 3.2 0 0 1-3.2 3.2H10a2.8 2.8 0 0 1-2.4-1.4L5.4 12.4 7 11.4z',
  // A note with a folded corner. The fold is what tells it from *Frame* at a glance.
  sticky: 'M4 4h12v7.5L11.5 16H4zM16 11.5h-4.5V16',
  // A T with a serif foot, which is how every application in the world draws this.
  text: 'M4.5 5h11M10 5v10M7.5 15h5',
  // A square and a circle. Miro's, and it says "some shape" rather than naming one.
  shape: 'M3 3.5h7.5V11H3zM12.7 8.6a4 4 0 1 1 0 8 4 4 0 0 1 0-8z',
  // Crop marks. Miro's frame glyph, and the same subject as the Velm mark.
  frame: 'M6.5 3v14M13.5 3v14M3 6.5h14M3 13.5h14',
  // A nib, with the ink edge across its heel.
  pen: 'M3.5 16.5l1.3-4L13.7 3.3l2.7 2.7-9.2 9.2zM12.2 4.8l2.7 2.7',
  // A block rubbing along a line, which is what makes it an eraser and not a shape.
  eraser: 'M3.2 12.5l7.3-7.3 5.3 5.3-5 5H6.4zM16.5 16.5H8.5M6.9 8.8l5.3 5.3',
  // An elbow, ending in an arrowhead: the connector's own subject.
  connector: 'M3.5 5h4.5a3 3 0 0 1 3 3v6M8.5 11.5l2.5 3 2.5-3',
  // The rest, as three marks. Zero-length subpaths do not render under `square` caps, so
  // each dot is a short segment rather than a point.
  more: 'M4 10h1.6M9.2 10h1.6M14.4 10h1.6',
  // The same three marks, turned: the `⋮` on the selection bar, as geometry.
  moreVertical: 'M10 4v1.6M10 9.2v1.6M10 14.4v1.6',
  undo: 'M6.5 5.5L3 9l3.5 3.5M3 9h8a4 4 0 0 1 0 8H7',
  redo: 'M13.5 5.5L17 9l-3.5 3.5M17 9H9a4 4 0 0 0 0 8h4',
  lock: 'M5.5 9h9v7.5h-9zM7.5 9V6.5a2.5 2.5 0 0 1 5 0V9',
  // The shackle open at one side, which is the only difference a reader can see at 20px.
  unlock: 'M5.5 9h9v7.5h-9zM7.5 9V6.5a2.5 2.5 0 0 1 5 0',
  // A droplet, for opacity. A checkerboard is the other convention and is unreadable at 20.
  opacity: 'M10 3.2l4.2 5.4a5.3 5.3 0 1 1-8.4 0z',
  // A pen line of varying weight, for the stroke width.
  strokeWidth: 'M3.5 6h13M3.5 10h13M3.5 14.5h13',
};

/// One 20-grid icon as inline SVG.
///
/// `aria-hidden`, because every button carries an `aria-label` — a screen reader announcing
/// both would say the name twice.
function svg(path) {
  return '<svg viewBox="0 0 20 20" width="20" height="20" fill="none" aria-hidden="true">'
    + `<path d="${path}" stroke="currentColor" stroke-width="1.5"`
    + ' stroke-linecap="square" stroke-linejoin="miter"/></svg>';
}

// ---------------------------------------------------------------------------------------
// Style
// ---------------------------------------------------------------------------------------
//
// `chrome.js`'s tokens exactly — `#22282C`, `#656D73`, `#E2E7EA`, `#F2F5F6`, `#E9EEF0`,
// `#8B959B`, `#6FD6E6`, `#00A38C`. Not `theme.rs`'s, for the reason that file gives: this
// design draws its structure with hairlines, so two nearly identical hairlines on one screen
// read as a mistake rather than as a system. There is no `prefers-color-scheme` block, for
// the same reason there is none there: the page has none, and light mode is the decision.

const CSS = `
.velm-tools,
.velm-tools-bar,
.velm-tools-menu,
.velm-tools-pop {
  font: 13px/1.4 -apple-system, BlinkMacSystemFont, "Segoe UI", Inter, sans-serif;
  color: #22282C;
  background: #fff;
  border: 1px solid #E2E7EA;
  border-radius: 10px;
  box-shadow: 0 1px 2px rgba(26, 29, 31, .06), 0 2px 8px rgba(26, 29, 31, .08);
  -webkit-user-select: none;
  user-select: none;
}

/* ⚠ The palette floats over the board and the board runs underneath it, which is only
   affordable because the container itself is transparent to a pointer. A touch pointer gets
   implicit capture on whatever it pressed, so one finger of a pinch landing on dead chrome
   sends that finger's whole stream here, the canvas sees one contact, and the board pans
   when the user meant to zoom. Every rule below that takes it back names a control. */
.velm-tools {
  position: fixed;
  left: max(12px, env(safe-area-inset-left));
  top: 50%;
  transform: translateY(-50%);
  display: flex;
  flex-direction: column;
  align-items: center;
  gap: 2px;
  padding: 4px;
  pointer-events: none;
  /* ⚠ A short window must not put the last tool under the bottom edge. Ten 44px targets and
     four rules is ~470px, which does not fit a phone held in landscape — and the obvious fix,
     making the column scroll, does not work on the surface it is needed for: this container
     is pointer-events: none so a finger cannot grab it to scroll, and giving it back the
     pointer is what makes the palette eat the board. So it **wraps into a second column**
     instead, which keeps every target at 44 and every tool reachable with no gesture at all.
     The selection bar measures the palette's live rectangle rather than a constant width,
     which is what makes the wrapped column something it already knows how to avoid. */
  flex-wrap: wrap;
  max-height: calc(100vh - 128px);
}

/* The undo pair, detached below the column — docs/04-ui-reference.md §1's own layout. */
.velm-tools-undo {
  position: fixed;
  left: max(12px, env(safe-area-inset-left));
  display: flex;
  flex-direction: column;
  gap: 2px;
  padding: 4px;
  pointer-events: none;
}

.velm-tools-btn {
  pointer-events: auto;
  appearance: none;
  -webkit-appearance: none;
  display: inline-flex;
  align-items: center;
  justify-content: center;
  gap: 6px;
  /* 44px, Apple's own minimum. The desktop's controls are 32pt because they are aimed with
     a mouse; the design doc's "no oversized whitespace" is about layout, not hit targets. */
  min-width: 44px;
  height: 44px;
  flex: none;
  padding: 0;
  margin: 0;
  border: 0;
  border-radius: 8px;
  background: transparent;
  color: inherit;
  font: inherit;
  cursor: pointer;
  transition: background-color 120ms ease-out;
  -webkit-tap-highlight-color: transparent;
  touch-action: manipulation;
}

/* ⚠ Guarded, because :hover on a touchscreen latches — a tapped button keeps the wash until
   something else is tapped, so the palette shows a tool as pointed-at that nothing is
   pointing at, next to the one that is genuinely active. */
@media (hover: hover) {
  .velm-tools-btn:hover { background: #F2F5F6; }
  .velm-tools-btn[disabled]:hover { background: transparent; }
}
.velm-tools-btn:active { background: #E9EEF0; }
.velm-tools-btn:focus-visible { outline: 2px solid #6FD6E6; outline-offset: -2px; }
.velm-tools-btn[disabled] { color: #8B959B; cursor: default; }

/* The active tool, tinted rather than recoloured. §1's own description of Miro's palette,
   and the one place the accent is allowed on this surface: it means *the active tool*. */
.velm-tools-btn[aria-pressed='true'] { background: rgba(0, 163, 140, .12); color: #007A69; }
@media (hover: hover) {
  .velm-tools-btn[aria-pressed='true']:hover { background: rgba(0, 163, 140, .18); }
}

.velm-tools-btn svg { display: block; width: 20px; height: 20px; flex: none; }

.velm-tools-rule {
  flex: none;
  width: 20px;
  height: 1px;
  margin: 2px 0;
  background: #E2E7EA;
  pointer-events: none;
}

/* The floating selection bar. Opaque, like the menu and for the same reason: §3a's
   *legibility wins over the material, every time*, plus the performance clause — a
   backdrop-filter is a second full-screen composite every frame in the tab this port is
   already worst at. The desktop's is glass because the desktop has a blur budget. */
.velm-tools-bar {
  position: fixed;
  left: 0;
  top: 0;
  display: flex;
  align-items: center;
  /* 6, the desktop's own number since the user asked for "more whitespace". */
  gap: 6px;
  padding: 4px;
  pointer-events: none;
}
.velm-tools-bar[hidden] { display: none; }

.velm-tools-bar-rule {
  flex: none;
  width: 1px;
  height: 20px;
  background: #E2E7EA;
}

/* A colour control shows the colour. A swatch is the whole content of the choice — the
   argument CLAUDE.md feedback 24 makes about the accent picker, one level down. */
.velm-tools-swatch {
  pointer-events: none;
  width: 20px;
  height: 20px;
  border-radius: 50%;
  border: 1px solid rgba(26, 29, 31, .18);
  /* The chequer behind a translucent colour, so alpha is visible rather than merely stored.
     Two gradients rather than an image, so nothing is fetched. */
  background-image:
    linear-gradient(45deg, #DDE3E6 25%, transparent 25%, transparent 75%, #DDE3E6 75%),
    linear-gradient(45deg, #DDE3E6 25%, transparent 25%, transparent 75%, #DDE3E6 75%);
  background-size: 8px 8px;
  background-position: 0 0, 4px 4px;
  box-sizing: border-box;
}
.velm-tools-swatch i {
  display: block;
  width: 100%;
  height: 100%;
  border-radius: 50%;
}
/* "Mixed", and "nothing here has one". Drawn as a diagonal rather than left blank, because a
   blank swatch and a white one are the same picture. */
.velm-tools-swatch[data-state='mixed'] i {
  background-image: linear-gradient(135deg, #fff 0 45%, #8B959B 45% 55%, #1A1D1F 55%);
}

/* Menus and popovers. Above everything else this file draws, so a menu opened over the bar
   is not covered by the bar it was opened from. */
.velm-tools-menu,
.velm-tools-pop {
  position: fixed;
  left: 0;
  top: 0;
  z-index: 3;
  padding: 4px;
  min-width: 180px;
  max-width: min(280px, calc(100vw - 24px));
}
.velm-tools-pop { min-width: 0; }
.velm-tools-menu[hidden],
.velm-tools-pop[hidden] { display: none; }

.velm-tools-row {
  appearance: none;
  -webkit-appearance: none;
  display: flex;
  align-items: center;
  width: 100%;
  /* 44 here too. A menu row is the control a finger is most likely to miss, because the
     rows are stacked and the neighbour does something else. */
  min-height: 44px;
  box-sizing: border-box;
  padding: 0 12px;
  border: 0;
  border-radius: 6px;
  background: transparent;
  color: inherit;
  font: inherit;
  text-align: left;
  cursor: pointer;
  transition: background-color 120ms ease-out;
  -webkit-tap-highlight-color: transparent;
  touch-action: manipulation;
}
@media (hover: hover) {
  .velm-tools-row:hover { background: #F2F5F6; }
  .velm-tools-row[disabled]:hover { background: transparent; }
}
.velm-tools-row:active { background: #E9EEF0; }
.velm-tools-row:focus-visible { outline: 2px solid #6FD6E6; outline-offset: -2px; }
.velm-tools-row[disabled] { color: #8B959B; cursor: default; }
/* Destructive, and only destructive. Since the accent moved to teal, nothing else in the
   light interface is red — feedback 22 split those roles on purpose, because delete used to
   look like selected. */
.velm-tools-row[data-danger='true']:not([disabled]) { color: #C6423F; }

.velm-tools-row-icon { flex: none; display: inline-flex; margin-right: 8px; }
.velm-tools-row-icon svg { display: block; width: 20px; height: 20px; }

.velm-tools-sep {
  height: 1px;
  margin: 4px 8px;
  background: #E2E7EA;
}

.velm-tools-pop-body { padding: 6px; }
.velm-tools-grid { display: grid; grid-template-columns: repeat(8, 32px); gap: 4px; }
.velm-tools-chip {
  appearance: none;
  -webkit-appearance: none;
  width: 32px;
  height: 32px;
  padding: 0;
  border: 1px solid rgba(26, 29, 31, .14);
  border-radius: 6px;
  cursor: pointer;
  -webkit-tap-highlight-color: transparent;
  touch-action: manipulation;
}
.velm-tools-chip:focus-visible { outline: 2px solid #6FD6E6; outline-offset: 1px; }

.velm-tools-field {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 6px 2px 2px;
  color: #656D73;
  font: 12px/1.4 -apple-system, BlinkMacSystemFont, "Segoe UI", Inter, sans-serif;
}
.velm-tools-field input[type='range'] { flex: 1 1 auto; min-width: 120px; accent-color: #00A38C; }
.velm-tools-field output {
  flex: none;
  min-width: 4.5ch;
  text-align: right;
  /* §3: monospace for anything numeric, tabular figures so the digits do not jitter while
     a slider is being dragged. */
  font: 12px/1.4 ui-monospace, SFMono-Regular, Menlo, monospace;
  font-variant-numeric: tabular-nums;
}

@media (prefers-reduced-motion: reduce) {
  .velm-tools-btn, .velm-tools-row { transition: none; }
}
`;

function ensureStyle(doc) {
  if (doc.getElementById(STYLE_ID)) return;
  const style = doc.createElement('style');
  style.id = STYLE_ID;
  style.textContent = CSS;
  doc.head.append(style);
}

// ---------------------------------------------------------------------------------------
// Small builders
// ---------------------------------------------------------------------------------------

function el(doc, tag, className) {
  const node = doc.createElement(tag);
  if (className) node.className = className;
  return node;
}

/// The name each verb is exported under, in the order they are tried.
///
/// ⚠ **This is the reconciliation point between two halves written in parallel, and it is
/// deliberately one table rather than a rename scattered through the file.** The contract
/// this file was written to says `undo`; the wasm crate on disk today says `velm_undo`, and
/// `velm_selection_count` and `velm_history_state` are exports that contract never mentioned.
/// Guessing either way would have produced a build where every control is disabled and the
/// functions behind them exist — which is this repository's signature defect wearing a new
/// hat, and the reason `CLAUDE.md` says to grep for the caller.
///
/// The **first** name is canonical: it is what a tooltip names when nothing matches, so a
/// reader is told which function to write rather than which of two spellings was tried.
const EXPORTS = {
  set_tool: ['set_tool', 'velm_set_tool'],
  selection_summary: ['selection_summary', 'velm_selection_summary'],
  style_selection: ['style_selection', 'velm_style_selection'],
  context_target: ['context_target', 'velm_context_target'],
  select_all: ['select_all', 'velm_select_all'],
  /// Taking the board out of the tab. Both optional: a build without them draws the rows
  /// disabled with a tooltip, which is how a reader finds out what this build cannot do.
  export_board: ['export_board', 'velm_export_board'],
  export_placeholders: ['export_placeholders', 'velm_export_placeholders'],
  delete_selection: ['delete_selection', 'velm_delete_selection'],
  cut_selection: ['cut_selection', 'velm_cut_selection'],
  copy_selection: ['copy_selection', 'velm_copy_selection'],
  duplicate_selection: ['duplicate_selection', 'velm_duplicate_selection'],
  paste: ['paste', 'velm_paste'],
  bring_to_front: ['bring_to_front', 'velm_bring_to_front'],
  send_to_back: ['send_to_back', 'velm_send_to_back'],
  lock_selection: ['lock_selection', 'velm_lock_selection'],
  undo: ['undo', 'velm_undo'],
  redo: ['redo', 'velm_redo'],
  /// The board's own editing switch — see the `editing` option on [`mountTools`].
  set_editing: ['set_editing', 'velm_set_editing'],
  /// Optional, both of them, and both already on disk. `selection_count` is the degraded
  /// answer to "is anything selected" when there is no summary to fold; `history_state` is
  /// `"<can_undo> <can_redo>"`, written by the Rust side with the comment *"for a page that
  /// greys its own buttons"* — which is this page.
  selection_count: ['selection_count', 'velm_selection_count'],
  history_state: ['history_state', 'velm_history_state'],
  /// The camera, for turning the summary's world-space box into a place on the screen.
  camera_report: ['camera_report', 'velm_camera_report'],
};

/** The name this module actually exports for a verb, or `null`. */
function bound(mod, name) {
  for (const candidate of EXPORTS[name] || [name]) {
    if (typeof mod[candidate] === 'function') return candidate;
  }
  return null;
}

/**
 * A button, wired only if the module can answer it.
 *
 * ⚠ The whole of the "nothing is inert" rule lives here. `fn` names the wasm export; when
 * the module has not got it the button is drawn, **disabled**, and its tooltip names the
 * function that is missing. A control that is simply absent leaves the reader wondering
 * whether they mis-remembered, and one that is present and silent is worse than both.
 */
function control(doc, { className, label, hint, icon, fn, mod, run, pressed }) {
  const node = el(doc, 'button', className);
  node.type = 'button';
  node.setAttribute('aria-label', label);
  if (icon) node.innerHTML = svg(icon);
  if (pressed !== undefined) node.setAttribute('aria-pressed', String(pressed));

  const available = !fn || Boolean(bound(mod, fn));
  if (available) {
    node.title = hint || label;
    node.addEventListener('click', run);
  } else {
    node.disabled = true;
    // Names the export rather than apologising vaguely. Somebody reading this tooltip is
    // either the user, who learns that the feature is not in this build, or the person
    // building the wasm half, who learns exactly which function to write.
    node.title = `${label}: this build has no ${fn}()`;
  }
  return node;
}

function divider(doc, className) {
  const rule = el(doc, 'span', className);
  rule.setAttribute('aria-hidden', 'true');
  return rule;
}

/**
 * A colour off the wire, as `{ hex, alpha }`.
 *
 * ⚠ **Six digits and a separate byte, and an eight-digit `#rrggbbaa` is refused.**
 * `style.rs`'s `Colour` says why in as many words: *"two places to say alpha is two places
 * that can disagree about it, and silently picking a winner is how a picker ends up
 * reporting a colour nobody chose."* This file agreed with itself about `#rrggbbaa` for one
 * afternoon and the crate on disk would have rejected every edit it sent — which is the
 * whole argument for reading the wire type rather than inventing one.
 *
 * Anything that is not a colour answers `null` rather than a guess: this is JSON off the
 * wasm boundary, and a malformed value painting a black swatch says "this item is black"
 * about an item that is not.
 */
function asColor(value) {
  if (!value || typeof value !== 'object') return null;
  const hex = typeof value.hex === 'string' ? value.hex.trim() : '';
  if (!/^#?(?:[0-9a-f]{3}|[0-9a-f]{6})$/i.test(hex)) return null;
  const six = hex.startsWith('#') ? hex.slice(1) : hex;
  const full = six.length === 3 ? six.split('').map(c => c + c).join('') : six;
  const alpha = Number.isFinite(Number(value.alpha)) ? Math.min(255, Math.max(0, Math.round(Number(value.alpha)))) : 255;
  return { hex: `#${full.toLowerCase()}`, alpha };
}

/** A wire colour as a CSS colour, for painting the swatch. */
function asCss(color) {
  if (!color) return null;
  return color.hex + color.alpha.toString(16).padStart(2, '0');
}

/** A number from the summary, clamped, or `fallback` when the field is not one. */
function asNumber(value, fallback, low, high) {
  const n = Number(value);
  if (!Number.isFinite(n)) return fallback;
  return Math.min(high, Math.max(low, n));
}

// ---------------------------------------------------------------------------------------
// mountTools
// ---------------------------------------------------------------------------------------

/**
 * Mount the editing chrome and wire it to the wasm module.
 *
 * `mod` is the module namespace object — the same one `index.html` holds as `velm`, and the
 * same one `mountChrome` is handed. Which controls are live is decided entirely by which
 * functions it exports; see the header for the disabled-versus-absent split.
 *
 * Options:
 *   - `canvas`   the board's `<canvas>`. Required: the surfaces are inserted after it and
 *                the right button and the long press are listened for on it.
 *   - `document` the document to build in. Defaults to `canvas.ownerDocument`, and exists
 *                so a DOM shim can drive this file with no browser.
 *   - `editing`  whether mounting this chrome also throws the viewer's own editing switch.
 *                Defaults to `true`, and see below.
 *
 * ⚠ **`editing`, and why it is an option rather than a decision made here.** `velm_set_editing`
 * changes what a one-finger drag does — with it on, a drag moves an item or sweeps a marquee
 * instead of panning — and the export's own documentation calls it *"a switch a person throws
 * rather than a default"*. It is right about that and this file is not the person. What it is,
 * is the only surface in the browser client from which that intent can be expressed: a palette
 * that arms a tool the viewer then ignores is a described gesture the user cannot perform,
 * which is the failure `CLAUDE.md` ranks worst. So the resolution is that **mounting the
 * editing chrome is the gesture**, and the caller — which is the page, and is not this file —
 * can decline it with `editing: false` and put the switch wherever it prefers. Unmounting puts
 * it back, so a page that takes the chrome away does not leave the board in a mode with no
 * controls for it.
 *
 * Returns `{ palette, undo, bar, menu, unmount }`, or `null` when there is no canvas. The
 * handle is an object rather than `mountChrome`'s single element because this mounts four
 * surfaces and three window listeners, and a caller that wants them gone should not have to
 * know that.
 */
export function mountTools(mod, { canvas, document: docOption, editing = true } = {}) {
  if (!mod || !canvas) return null;
  const doc = docOption || canvas.ownerDocument;
  if (!doc) return null;
  const win = doc.defaultView || (typeof window !== 'undefined' ? window : null);
  if (!win) return null;

  // ⚠ Idempotent, and it has to be. Without this a second call stacks a second palette, a
  // second `keydown` on the window — so one `n` arms the sticky tool twice — and a second
  // rAF poll writing into a bar that has left the document.
  if (mounted) mounted.unmount();

  ensureStyle(doc);

  // Truthy when the module exports the verb, and the *actual* name when it does — so
  // every call site below goes through one lookup and no site spells a name twice.
  const can = (name) => bound(mod, name);
  const invoke = (name, ...args) => {
    const fn = bound(mod, name);
    return fn ? mod[fn](...args) : undefined;
  };

  // ------------------------------------------------------------------ shared state

  const state = {
    live: true,
    /// The armed tool. Local, because there is no `current_tool()` to ask — see the report.
    /// Seeded to `select`, which is what a board opens in on the desktop.
    tool: 'select',
    /// The open menu, popover and their fresh guards.
    menuOpen: false,
    popOpen: null,
    /// ⚠ **The fresh guard, and it is the DOM's version of `context_menu.rs`'s `Opened::fresh`.**
    /// The gesture that opens a menu is not finished when the menu appears: the `pointerup`
    /// and the `click` that follow the opening `contextmenu` land *outside* a rectangle that
    /// did not exist when the press began, so the outside-click closer fires on the opener
    /// itself and the menu opens and closes without ever being seen. It cost a round of "the
    /// right button does nothing" on the desktop. Everything that closes a surface ignores
    /// the guard's window, which is cleared on the next turn of the event loop.
    guard: false,
    /// True between a long press firing and the `contextmenu` some platforms send after it,
    /// so Android does not open the menu twice from one gesture.
    pressedRecently: 0,
  };

  const openGuard = () => {
    state.guard = true;
    win.setTimeout(() => { state.guard = false; }, 0);
  };

  // ------------------------------------------------------------------ the palette

  const palette = el(doc, 'div', 'velm-tools');
  palette.setAttribute('role', 'toolbar');
  palette.setAttribute('aria-label', 'Tools');
  palette.setAttribute('aria-orientation', 'vertical');

  const toolButtons = new Map();
  const showArmed = (id) => {
    state.tool = id;
    for (const [key, node] of toolButtons) node.setAttribute('aria-pressed', String(key === id));
  };
  const armTool = (id) => {
    if (!can('set_tool')) return;
    // ⚠ **The answer is the refusal.** Two of the fourteen tools are not in this build, and a
    // button that goes pressed for a tool that did not arm is this file's own prohibited
    // shape: an enabled-looking control for a gesture that will not happen.
    if (invoke('set_tool', id) === false) return;
    // Arming a tool ends a text session. The caret holds an undo group open for its whole
    // life, so anything that starts a new gesture has to close it — feedback 27's rule, and
    // the third place in this application that has had to learn it.
    if (can('velm_caret_commit')) invoke('velm_caret_commit');
    showArmed(id);
  };

  for (const group of TOOL_GROUPS) {
    if (palette.childElementCount) palette.append(divider(doc, 'velm-tools-rule'));
    for (const tool of group) {
      const hint = tool.key ? `${tool.label} (${tool.key.toUpperCase()})` : tool.label;
      const node = control(doc, {
        className: 'velm-tools-btn',
        label: tool.label,
        hint,
        icon: ICONS[tool.id],
        fn: 'set_tool',
        mod,
        pressed: tool.id === state.tool,
        run: () => armTool(tool.id),
      });
      toolButtons.set(tool.id, node);
      palette.append(node);
    }
  }

  // **More**, and it opens a list rather than being one. The five behind it are the
  // desktop's `Tool::OCCASIONAL`; none has a key, there or here.
  palette.append(divider(doc, 'velm-tools-rule'));
  const moreButton = control(doc, {
    className: 'velm-tools-btn',
    label: 'More tools',
    hint: 'More tools',
    icon: ICONS.more,
    fn: 'set_tool',
    mod,
    run: () => openMoreTools(),
  });
  palette.append(moreButton);

  // The undo pair, detached below the column — §1's own layout, and the reason it is here at
  // all: `undo` and `redo` are exports, and an export with no caller is this repository's
  // signature defect.
  const undo = el(doc, 'div', 'velm-tools-undo');
  undo.setAttribute('role', 'group');
  undo.setAttribute('aria-label', 'History');
  const undoButton = control(doc, {
    className: 'velm-tools-btn',
    label: 'Undo',
    hint: 'Undo',
    icon: ICONS.undo,
    fn: 'undo',
    mod,
    run: () => invoke('undo'),
  });
  const redoButton = control(doc, {
    className: 'velm-tools-btn',
    label: 'Redo',
    hint: 'Redo',
    icon: ICONS.redo,
    fn: 'redo',
    mod,
    run: () => invoke('redo'),
  });
  undo.append(undoButton, redoButton);

  /**
   * Greys the pair when there is nothing to undo or redo.
   *
   * `docs/04-ui-reference.md` §6 lists it as one of Miro's conventions to match — *"Redo
   * greys out when unavailable"* — and the export exists for it: `velm_history_state`'s own
   * doc says it is *"for a page that greys its own buttons"*.
   *
   * ⚠ **A button whose export is missing is left alone.** It is already disabled with a
   * tooltip naming the function that is not there, and re-enabling it here — or overwriting
   * that tooltip with *"Nothing to undo"* — would replace a true sentence about the build
   * with a false one about the board.
   */
  function refreshHistory() {
    if (!can('history_state')) return;
    let line;
    try {
      line = String(invoke('history_state'));
    } catch (error) {
      return;
    }
    const [back, forward] = line.trim().split(/\s+/);
    const set = (node, verb, ok, empty) => {
      if (!can(verb)) return;
      node.disabled = !ok;
      node.title = ok ? node.getAttribute('aria-label') : empty;
    };
    set(undoButton, 'undo', back === '1', 'Nothing to undo');
    set(redoButton, 'redo', forward === '1', 'Nothing to redo');
  }

  // ------------------------------------------------------------------ the menu

  const menu = el(doc, 'div', 'velm-tools-menu');
  menu.setAttribute('role', 'menu');
  menu.hidden = true;

  /**
   * Reads the selection, defensively.
   *
   * ⚠ Guarded whole. `panic = "abort"` poisons a wasm module, so once one export throws
   * every later one does — and this is polled every frame. An unguarded read would put a
   * throw inside the rAF chain, which stops the chain, which freezes the bar wherever it
   * last was: a selection toolbar hovering over a board that has stopped answering. The
   * same argument `chrome.js` makes about its own status timer.
   */
  const summary = () => {
    if (!can('selection_summary')) return null;
    try {
      const answer = invoke('selection_summary');
      const parsed = typeof answer === 'string' ? JSON.parse(answer) : answer;
      return parsed && typeof parsed === 'object' ? parsed : null;
    } catch (error) {
      return null;
    }
  };

  function closeMenu() {
    if (!state.menuOpen) return;
    state.menuOpen = false;
    menu.hidden = true;
    menu.textContent = '';
  }

  /**
   * The route a row would take, or `null` when this build offers none of them.
   *
   * Answers `{ fn, arg }` — the *actual* exported name and the argument that route wants —
   * so the caller never has to know there were two candidates.
   */
  function route(row) {
    if (!row) return null;
    for (const [name, arg] of row.via) {
      const fn = can(name);
      if (fn) return { fn, arg: arg ? arg() : undefined };
    }
    return null;
  }

  /** The canonical export a row would like, for a tooltip that names what to write. */
  const wants = (row) => row.via[0][0];

  function runRow(key) {
    const row = ROWS[key];
    closeMenu();
    const taken = route(row);
    if (!taken) return;
    const answer = taken.arg === undefined ? mod[taken.fn]() : mod[taken.fn](taken.arg);
    if (row.download) deliver(answer, row.download);
  }

  function buildMenu(target, model) {
    menu.textContent = '';
    for (const key of menuRows(target, model)) {
      if (key === 'sep') {
        menu.append(divider(doc, 'velm-tools-sep'));
        continue;
      }
      const row = ROWS[key];
      const node = el(doc, 'button', 'velm-tools-row');
      node.type = 'button';
      node.setAttribute('role', 'menuitem');
      // ⚠ `textContent`, always. Nothing on this menu comes off the wire today, and that is
      // exactly the state in which the habit is cheap to keep — the first row that names a
      // board or an item's own words is the one that would otherwise carry markup home.
      node.textContent = row.label;
      if (row.danger) node.dataset.danger = 'true';
      if (route(row)) {
        node.addEventListener('click', () => runRow(key));
      } else {
        node.disabled = true;
        node.title = `${row.label}: this build has no ${wants(row)}()`;
      }
      menu.append(node);
    }
    // No shortcut column, deliberately. Miro's menu shows `⌘C` beside Copy and the desktop's
    // does too, because both bind it; nothing in this file or in the wasm module binds a
    // clipboard chord, so printing one would describe a gesture the reader cannot perform —
    // which `CLAUDE.md` names the worst failure in its list. The column comes back with the
    // bindings and not before.
  }

  /** Places a floating surface at a point, clamped so none of it is off screen. */
  function placeAt(node, x, y) {
    node.hidden = false;
    const box = node.getBoundingClientRect();
    const w = box.width || node.offsetWidth || 200;
    const h = box.height || node.offsetHeight || 200;
    const maxX = Math.max(GAP, (win.innerWidth || 0) - w - GAP);
    const maxY = Math.max(GAP, (win.innerHeight || 0) - h - GAP);
    node.style.left = `${Math.min(Math.max(GAP, x), maxX)}px`;
    node.style.top = `${Math.min(Math.max(GAP, y), maxY)}px`;
  }

  /// Say something to the person, through the page's own status line.
  ///
  /// ⚠ Deliberately not a `window.alert`, and not silence either. An alert is modal and steals
  /// the keyboard, which on a tablet also dismisses the caret; silence is how a person finds
  /// out an export was partial by opening the file. The status line is already the page's one
  /// channel for this — it is what says `Starting…` and what the render proof writes into —
  /// and it is looked at, because it is where the item count lives.
  const say = (message) => {
    const line = doc.getElementById('velm-status');
    if (line) {
      line.hidden = false;
      line.textContent = message;
    } else {
      console.log(message);
    }
  };

  /// Hand the browser a file.
  ///
  /// ⚠ `URL.revokeObjectURL` is not optional housekeeping. A blob URL pins its bytes for the
  /// life of the *document*, and an SVG of a 1,300-item board is megabytes — so a session
  /// spent exporting would hold every export it ever made, in a tab whose linear memory never
  /// returns to the operating system anyway. The timeout is because revoking synchronously
  /// races the download the click just started.
  const handOver = (text, name, media) => {
    const url = URL.createObjectURL(new Blob([text], { type: media }));
    const link = doc.createElement('a');
    link.href = url;
    link.download = name;
    link.style.display = 'none';
    doc.body.append(link);
    link.click();
    link.remove();
    win.setTimeout(() => URL.revokeObjectURL(url), 10_000);
  };

  /// Take what an export row answered and give it to the browser.
  ///
  /// Separate from the row's own invocation because the caveat depends on the file: a
  /// spreadsheet carries no pictures on any platform, so warning about them there would be
  /// a warning about nothing.
  const deliver = (text, spec) => {
    // ⚠ Empty means nothing came out — no viewer yet, an empty board, an emitter that
    // refused. Handing over a zero-byte file with the right name is the shape this
    // application refuses everywhere else: it looks exactly like success until it is opened.
    if (typeof text !== 'string' || text.length === 0) {
      say('Nothing came out. This board has nothing to export.');
      return;
    }
    handOver(text, spec.name, spec.media);
    const missing = spec.warnsAboutPictures ? Number(invoke('export_placeholders') || 0) : 0;
    // Said after the download rather than as a dialog before it: the file is the honest thing
    // to hand over, and the caveat is about the pictures alone.
    if (missing > 0) {
      say(
        `Downloaded. ${missing} picture${missing === 1 ? '' : 's'} exported as placeholders, because ` +
        'a browser holds them as textures, not as bytes, so there are no pixels here to write.',
      );
    } else {
      say(`Downloaded ${spec.name}.`);
    }
  };

  function openMenu(x, y) {
    closePopover();
    // Where a paste lands. Defensive, like `bound()`: the page must keep working against a
    // wasm build that has not got this export. Without it a paste falls back to the viewport
    // centre — on screen and honest, but not where the user is pointing.
    if (typeof mod.velm_note_paste_aim === 'function') mod.velm_note_paste_aim(x, y);
    // ⚠ `context_target` does the selecting, because only wasm knows the scene. A
    // right-click on an **unselected** item selects it first; one **inside** an existing
    // selection leaves it alone, so right-clicking one of five picked items still offers all
    // five; and a right-click on bare board does **not** clear the selection — a request for
    // a menu is not a click. All three rules live on the far side of this call, which is why
    // it is one call and not a hit test up here.
    let target;
    if (can('context_target')) {
      try {
        target = invoke('context_target', x, y);
      } catch (error) {
        target = null;
      }
    }
    const model = summary();
    if (target !== 'selection' && target !== 'canvas') {
      // Degraded, and honestly: with no `context_target` the list is chosen from whether
      // anything is *already* selected, and the select-first rule above simply does not
      // happen — a right-click on an unselected item offers the canvas rows. Better than
      // refusing to open, and named in the report rather than left to be discovered.
      // `selection_count` is the fallback's fallback: a build with neither a summary nor a
      // target resolver still knows how many items are picked.
      const count = model && Number.isFinite(model.count)
        ? model.count
        : Number(invoke('selection_count')) || 0;
      target = count ? 'selection' : 'canvas';
    }
    buildMenu(target, model);
    state.menuOpen = true;
    placeAt(menu, x, y);
    openGuard();
  }

  // ------------------------------------------------------------------ popovers

  const pop = el(doc, 'div', 'velm-tools-pop');
  pop.hidden = true;

  function closePopover() {
    if (!state.popOpen) return;
    state.popOpen = null;
    pop.hidden = true;
    pop.textContent = '';
  }

  function openPopover(anchor, build) {
    closeMenu();
    // Pressing the control a second time puts the popover away, which is what
    // `ContextBarState::open_picker` does on the desktop. Without it the outside-closer
    // skips the anchor (it is not "outside") and the handler re-opens, so the popover
    // could be opened and never dismissed from the control that opened it.
    if (state.popOpen === anchor) {
      closePopover();
      return;
    }
    pop.textContent = '';
    build(pop);
    state.popOpen = anchor;
    pop.hidden = false;
    const from = anchor.getBoundingClientRect();
    const box = pop.getBoundingClientRect();
    const h = box.height || pop.offsetHeight || 120;
    // Below the control, unless the control is low enough that below would be off screen.
    const under = from.bottom + GAP;
    const y = under + h + GAP > (win.innerHeight || 0) ? from.top - GAP - h : under;
    placeAt(pop, from.left, y);
    openGuard();
  }

  function openMoreTools() {
    openPopover(moreButton, (host) => {
      const body = el(doc, 'div', 'velm-tools-pop-body');
      for (const tool of MORE_TOOLS) {
        const node = el(doc, 'button', 'velm-tools-row');
        node.type = 'button';
        node.textContent = tool.label;
        if (can('set_tool')) {
          node.addEventListener('click', () => { closePopover(); armTool(tool.id); });
        } else {
          node.disabled = true;
          node.title = `${tool.label}: this build has no set_tool()`;
        }
        body.append(node);
      }
      host.append(body);
    });
  }

  /**
   * Applies one style field.
   *
   * One key per call, so the Rust side maps one field to one edit and a change can never be
   * observed half-applied — `vellum_doc::Color` packs to a single integer for exactly this
   * reason. Guarded because the poll behind it is a rAF chain, as above.
   */
  function style(patch) {
    if (!can('style_selection')) return;
    try {
      invoke('style_selection', JSON.stringify(patch));
    } catch (error) {
      /* A failed edit is the wasm side's to report; a throw here would stop the bar. */
    }
  }

  /**
   * The board's swatches, plus the alpha that rides beside them, plus *no fill*.
   *
   * `apply` is handed a wire colour — `{hex, alpha}` — or `null` for the clear verb, which is
   * `StyleEdit::Fill(None)` on the far side. Clearing is a real choice a shape can make and a
   * sticky cannot, which is why it is a chip here rather than an absence.
   */
  function swatchPopover(anchor, current, apply, { clearable }) {
    openPopover(anchor, (host) => {
      const body = el(doc, 'div', 'velm-tools-pop-body');
      const grid = el(doc, 'div', 'velm-tools-grid');
      // The alpha of whatever is there now is carried across every swatch press, or picking a
      // colour would silently undo the slider next door — the rule CLAUDE.md feedback 31
      // states for the grid's own colour list, one level down.
      const alpha = current ? current.alpha : 255;
      for (const row of SWATCHES) {
        for (const hex of row) {
          const chip = el(doc, 'button', 'velm-tools-chip');
          chip.type = 'button';
          chip.style.background = hex;
          chip.title = hex;
          chip.setAttribute('aria-label', hex);
          chip.addEventListener('click', () => {
            closePopover();
            apply({ hex: hex.toLowerCase(), alpha });
          });
          grid.append(chip);
        }
      }
      body.append(grid);

      if (clearable) {
        const clear = el(doc, 'button', 'velm-tools-row');
        clear.type = 'button';
        clear.textContent = 'No fill';
        clear.addEventListener('click', () => { closePopover(); apply(null); });
        body.append(clear);
      }

      const field = el(doc, 'div', 'velm-tools-field');
      const name = el(doc, 'span');
      name.textContent = 'Alpha';
      const range = doc.createElement('input');
      range.type = 'range';
      range.min = '0';
      range.max = '100';
      range.step = '1';
      range.value = String(Math.round((alpha / 255) * 100));
      // ⚠ Nothing to be the alpha *of*. With no colour set — a shape whose fill is `unset`,
      // which is a real state and not a missing one — the slider's only possible answer is
      // black at some opacity, which is a colour nobody chose. Pick a swatch first; the
      // slider is live from the moment there is something to fade.
      if (!current) {
        range.disabled = true;
        range.title = 'Pick a colour first';
      }
      const out = doc.createElement('output');
      out.textContent = `${range.value}%`;
      range.addEventListener('input', () => { out.textContent = `${range.value}%`; });
      // On `change`, not on `input`: a slider dragged across its range would otherwise be a
      // hundred writes, which is a hundred undo steps for one gesture.
      range.addEventListener('change', () => {
        apply({
          hex: current ? current.hex : '#000000',
          alpha: Math.round((Number(range.value) / 100) * 255),
        });
      });
      field.append(name, range, out);
      body.append(field);
      host.append(body);
    });
  }

  function sliderPopover(anchor, { title, min, max, step, value, unit, apply }) {
    openPopover(anchor, (host) => {
      const body = el(doc, 'div', 'velm-tools-pop-body');
      const field = el(doc, 'div', 'velm-tools-field');
      const name = el(doc, 'span');
      name.textContent = title;
      const range = doc.createElement('input');
      range.type = 'range';
      range.min = String(min);
      range.max = String(max);
      range.step = String(step);
      range.value = String(value);
      const out = doc.createElement('output');
      out.textContent = `${range.value}${unit}`;
      range.addEventListener('input', () => { out.textContent = `${range.value}${unit}`; });
      range.addEventListener('change', () => apply(Number(range.value)));
      field.append(name, range, out);
      body.append(field);
      host.append(body);
    });
  }

  // ------------------------------------------------------------------ the selection bar

  const bar = el(doc, 'div', 'velm-tools-bar');
  bar.setAttribute('role', 'toolbar');
  bar.setAttribute('aria-label', 'Selection');
  bar.hidden = true;

  /** A swatch button: the colour is the content of the choice. */
  function swatchButton(label, field, apply, { clearable = false } = {}) {
    const node = el(doc, 'button', 'velm-tools-btn');
    node.type = 'button';
    node.setAttribute('aria-label', label);
    const chip = el(doc, 'span', 'velm-tools-swatch');
    const ink = el(doc, 'i');
    chip.append(ink);
    node.append(chip);
    // ⚠ Three states, not two, and `unset` is the one a two-state reader loses. `style.rs`
    // spells them `uniform` / `unset` / `mixed`: **`unset` is "they agree, and the document
    // says nothing"** — a shape with no fill — which is a real choice and not the same as
    // *nothing here has a fill at all*, which is the key being omitted entirely. A swatch
    // painted from `valueOr(field, black)` would report a colour nobody chose.
    const colour = field && field.state === 'uniform' ? asColor(field.value) : null;
    if (field && field.state === 'mixed') chip.dataset.state = 'mixed';
    else ink.style.background = asCss(colour) || 'transparent';
    if (can('style_selection')) {
      node.title = label;
      node.addEventListener('click', () => swatchPopover(node, colour, apply, { clearable }));
    } else {
      node.disabled = true;
      node.title = `${label}: this build has no style_selection()`;
    }
    return node;
  }

  /// The bar's own memory, so it is rebuilt only when what it should hold has changed.
  /// A pan moves the selection sixty times a second and changes none of this; without the
  /// key the bar would be torn down and rebuilt on every one of those frames, which also
  /// closes any popover open on it mid-drag.
  let barKey = '';

  function buildBar(model) {
    bar.textContent = '';
    for (const id of barControls(model)) {
      if (id === 'sep') {
        bar.append(divider(doc, 'velm-tools-bar-rule'));
        continue;
      }
      if (id === 'fill') {
        bar.append(swatchButton('Fill colour', model.fill, (c) => style({ fill: c }), { clearable: true }));
      } else if (id === 'stroke') {
        bar.append(swatchButton('Line colour', model.stroke, (c) => style({ stroke: c }), { clearable: true }));
      } else if (id === 'strokeWidth') {
        const width = asNumber(model.stroke_width && model.stroke_width.value, 2, 1, 24);
        bar.append(control(doc, {
          className: 'velm-tools-btn',
          label: 'Line width',
          hint: 'Line width',
          icon: ICONS.strokeWidth,
          fn: 'style_selection',
          mod,
          run: (event) => sliderPopover(event.currentTarget, {
            title: 'Width', min: 1, max: 24, step: 1, value: width, unit: 'px',
            apply: (v) => style({ stroke_width: v }),
          }),
        }));
      } else if (id === 'opacity') {
        const percent = Math.round(asNumber(model.opacity && model.opacity.value, 1, 0, 1) * 100);
        bar.append(control(doc, {
          className: 'velm-tools-btn',
          label: 'Opacity',
          hint: 'Opacity',
          icon: ICONS.opacity,
          fn: 'style_selection',
          mod,
          run: (event) => sliderPopover(event.currentTarget, {
            title: 'Opacity', min: 0, max: 100, step: 1, value: percent, unit: '%',
            apply: (v) => style({ opacity: v / 100 }),
          }),
        }));
      } else if (id === 'lock') {
        // One button showing the verb that applies, exactly as `context_menu.rs` argues: a
        // pair where one is always greyed out is noise on a bar this short. A **mixed**
        // selection locks, because that is the half of the choice that loses nothing.
        const locked = model.locked && model.locked.state === 'uniform' && model.locked.value === true;
        const key = locked ? 'unlock' : 'lock';
        const node = el(doc, 'button', 'velm-tools-btn');
        node.type = 'button';
        node.setAttribute('aria-label', ROWS[key].label);
        node.innerHTML = svg(locked ? ICONS.unlock : ICONS.lock);
        // ⚠ Through `runRow`, not through a call of its own. The padlock is a `StyleEdit` on
        // one build and a flat export on another, and a second copy of that choice here is
        // exactly the two-sources-of-truth failure this file's own comments keep citing —
        // with the specific consequence that the bar could lock through a route the menu
        // does not have, or refuse where the menu offers.
        if (route(ROWS[key])) {
          node.title = ROWS[key].label;
          node.addEventListener('click', () => runRow(key));
        } else {
          node.disabled = true;
          node.title = `${ROWS[key].label}: this build has no ${wants(ROWS[key])}()`;
        }
        bar.append(node);
      } else if (id === 'more') {
        // The `⋮`, opening the same list the right button gives. Never gated on an export:
        // the menu is always drawable — its own rows carry the honesty about what is
        // missing, and a `⋮` that refused to open would hide that from the one reader
        // trying to find out.
        const node = el(doc, 'button', 'velm-tools-btn');
        node.type = 'button';
        node.setAttribute('aria-label', 'More');
        node.title = 'More';
        node.innerHTML = svg(ICONS.moreVertical);
        node.addEventListener('click', () => {
          const from = node.getBoundingClientRect();
          openMenu(from.left, from.bottom + GAP);
        });
        bar.append(node);
      }
    }
  }

  /**
   * Puts the bar above the selection, and clear of every other surface on the page.
   *
   * There are three of those and none of them is this file's: `chrome.js`'s bar across the
   * top, this file's own tool palette down the left, and `inspect.js`'s toggle and panel down
   * the right. A floating toolbar underneath any of them is a row of controls that cannot be
   * clicked — which shipped on the desktop once, measured, with a selection at the board's
   * left edge putting the fill swatch behind the tool column.
   *
   * `bounds` is in **client CSS pixels** — see [`screenRect`], which is where a world-space
   * box from the summary is converted.
   */
  function placeBar(bounds) {
    // Measured live, every frame the bar is up, rather than cached at the rebuild. The
    // `getBoundingClientRect` reads cost microseconds and they all happen *before* any style
    // is written, so there is no read-write thrash; a cached rectangle would be one more
    // thing that can go stale, which is the class of bug this file's own comments keep
    // citing. The reads only happen while something is selected — an idle board pays none.
    const box = bar.getBoundingClientRect();
    const w = box.width || bar.offsetWidth || 0;
    const h = box.height || bar.offsetHeight || 0;
    const vw = win.innerWidth || 0;
    const vh = win.innerHeight || 0;

    /** A surface's rectangle, when it is on the page and drawn. */
    const surface = (selector) => {
      const node = doc.querySelector(selector);
      if (!node || node.hidden) return null;
      const rect = node.getBoundingClientRect();
      return rect.width > 0 && rect.height > 0 ? rect : null;
    };

    // ⚠ **The top band, unconditionally.** `chrome.js`'s bar is one strip across the top and
    // `inspect.js`'s toggle keeps the top-right corner; keeping a floating toolbar out of
    // that band costs a few pixels in a rare case. Unlike the two horizontal clamps below it
    // is not conditional, which also removes a circular dependency — those depend on the
    // vertical placement, and making this depend on them would close the loop.
    let top = GAP;
    for (const rect of [surface('.velm-chrome'), surface('.velm-inspect-toggle')]) {
      if (rect) top = Math.max(top, rect.bottom + GAP);
    }

    // Above, unless above would be behind that band or off the top — then below, which is the
    // same two placements `context_bar::show` picks between.
    //
    // ⚠ **`top` floors the *below* branch as well**, and the fixture is what found that it did
    // not. A selection tucked under the top edge has no room above it, so the bar goes below —
    // and `bounds.bottom + GAP` for a short selection is still inside the band the lines above
    // just reserved. It landed 4px clear of `chrome.js`'s bar instead of 8, which is the
    // placement bug this whole function exists to prevent, arriving through the other branch.
    // Overlapping the selection a little is the right side of that trade: a bar that covers
    // part of what it belongs to can still be pressed, and one behind the chrome cannot.
    const wantY = bounds.top - GAP - h;
    const below = Math.min(bounds.bottom + GAP, Math.max(top, vh - h - GAP));
    const y = wantY >= top ? wantY : Math.max(top, below);

    // ⚠ **Clear of the tool palette on the left and the properties panel on the right**, each
    // only when it would actually share a row with the bar at `y`. A selection well below the
    // palette has the whole width of the window, and pushing the bar aside regardless would
    // move it away from the thing it belongs to for no reason.
    //
    // Both are measured from a live `getBoundingClientRect()` rather than from a constant.
    // The palette's width follows the icon size and **wraps into a second column on a short
    // window**; the properties panel is `min(264px, 100vw - 24px)` and is not always up. A
    // constant for either goes stale silently, and the failure it produces is a row of
    // controls underneath another panel — measured on the desktop once, where a selection at
    // the board's left edge put the fill swatch behind the tool column.
    const shares = (rect) => rect && y < rect.bottom && y + h > rect.top;

    let left = GAP;
    const paletteBox = palette.getBoundingClientRect();
    if (paletteBox.width > 0 && shares(paletteBox)) left = paletteBox.right + GAP;

    let right = vw - w - GAP;
    const panelBox = surface('.velm-inspect');
    if (shares(panelBox)) right = Math.min(right, panelBox.left - GAP - w);

    const centred = bounds.left + (bounds.right - bounds.left) / 2 - w / 2;
    // `Math.max(left, right)` and not `right`: on a narrow window the two limits cross, and
    // the left one wins — a bar pushed off the right edge is still reachable by scrolling
    // nothing, whereas one under the tool palette cannot be pressed at all.
    const x = Math.min(Math.max(centred, left), Math.max(left, right));

    const nextLeft = `${Math.round(x)}px`;
    const nextTop = `${Math.round(y)}px`;
    // Written only on a change, the rule `chrome.js` states for its zoom readout: this runs
    // on every frame of every drag, and a style write per frame is a layout per frame.
    if (bar.style.left !== nextLeft) bar.style.left = nextLeft;
    if (bar.style.top !== nextTop) bar.style.top = nextTop;
  }

  /**
   * One pass of the bar.
   *
   * ⚠ Exported on the handle so a fixture can drive it without a frame loop: `refresh()` is
   * the whole of what the poll does, and a test that called the poll would be testing
   * `requestAnimationFrame`.
   */
  /**
   * The selection's box in **client CSS pixels**, which is the only space a fixed-position
   * element can be placed in.
   *
   * ⚠ **`selection_summary().bounds` is `{x, y, w, h}` in *world* units**, measured off
   * `style.rs`'s `Box4` — *"the selection's true world bounding box"* — and treating those
   * numbers as pixels puts the bar at the origin on a fitted board and hundreds of screens
   * away on a zoomed one. So it is converted here, from `camera_report()`, which answers
   * `"<zoom> <centre x> <centre y>"`.
   *
   * ⚠ **And the conversion has to divide by the device pixel ratio.** `Camera::world_to_screen`
   * works in the viewport's own units and the viewport is the canvas's *backing* size — 
   * physical pixels — while `getBoundingClientRect` and `style.left` are CSS pixels. On a
   * Retina display those differ by exactly 2, which is `CLAUDE.md` trap 4 restated for a
   * second target: *mixing logical and physical makes it drift by exactly the scale factor.*
   * Taking the centre from the canvas's own rect rather than from `viewport / 2` is what
   * removes the second place that could get it wrong.
   *
   * A build whose summary already reports `{left, top, right, bottom}` is taken at its word
   * and not converted — that is the shape this file was originally specified against, and the
   * two are told apart by their field names rather than by a flag.
   */
  function screenRect(bounds) {
    if (!bounds) return null;
    if (Number.isFinite(bounds.left) && Number.isFinite(bounds.top)) {
      return {
        left: bounds.left,
        top: bounds.top,
        right: Number.isFinite(bounds.right) ? bounds.right : bounds.left,
        bottom: Number.isFinite(bounds.bottom) ? bounds.bottom : bounds.top,
      };
    }
    if (!Number.isFinite(bounds.x) || !Number.isFinite(bounds.y)) return null;
    if (!can('camera_report')) return null;
    let report;
    try {
      report = String(invoke('camera_report'));
    } catch (error) {
      return null;
    }
    const [zoom, cx, cy] = report.split(/\s+/).map(Number);
    if (!Number.isFinite(zoom) || !Number.isFinite(cx) || !Number.isFinite(cy)) return null;
    const dpr = Math.max(1, Number(win.devicePixelRatio) || 1);
    const view = canvas.getBoundingClientRect();
    const scale = zoom / dpr;
    const left = view.left + view.width / 2 + (bounds.x - cx) * scale;
    const top = view.top + view.height / 2 + (bounds.y - cy) * scale;
    return {
      left,
      top,
      right: left + (Number(bounds.w) || 0) * scale,
      bottom: top + (Number(bounds.h) || 0) * scale,
    };
  }

  function refresh() {
    refreshHistory();
    const model = summary();
    const controls = barControls(model);
    const box = model ? screenRect(model.bounds) : null;
    if (!controls.length || !box) {
      if (!bar.hidden) {
        bar.hidden = true;
        bar.textContent = '';
        barKey = '';
        // A popover belonging to a bar that has gone is a control floating over nothing.
        if (state.popOpen && bar.contains(state.popOpen)) closePopover();
      }
      return;
    }
    // The key is what the bar *is*, not where it is: the controls it holds and the values
    // they display. Position is applied every pass regardless, which is what makes the bar
    // follow a drag without being rebuilt by one.
    const key = JSON.stringify([
      controls,
      model.fill, model.stroke, model.stroke_width, model.opacity, model.locked,
    ]);
    if (key !== barKey) {
      barKey = key;
      buildBar(model);
      // A rebuild replaces the button a popover was anchored to, so anything still open is
      // pointing at an element that has left the document.
      if (state.popOpen && !doc.contains(state.popOpen)) closePopover();
    }
    bar.hidden = false;
    placeBar(box);
  }

  // ------------------------------------------------------------------ listeners

  // ⚠ **The right button must not also pan the board.** `vellum_web::input`'s `pointerdown`
  // handler makes a contact for *any* button — only its `pointerup` filters on button 0 — so
  // without this a right-drag pans the board out from under the menu that the same gesture
  // just opened. Stopped in the **capture** phase at the window, which is the only place
  // that runs before a listener registered on the canvas itself: at the target, capture and
  // bubble listeners run in registration order, so a capture listener on the canvas would
  // not reliably win. `preventDefault` is deliberately not called — the `contextmenu` event
  // that follows is the one this file is waiting for.
  const onCapturePointerDown = (event) => {
    if (event.button === 2 && (event.target === canvas || canvas.contains(event.target))) {
      event.stopPropagation();
    }
  };

  const onContextMenu = (event) => {
    event.preventDefault();
    // Android fires `contextmenu` *after* the long press this file has already answered.
    // One gesture, one menu.
    if (Date.now() - state.pressedRecently < 700) return;
    openMenu(event.clientX, event.clientY);
  };

  // A long press is the right button on a touchscreen, where there is no right button.
  // ⚠ iOS fires no `contextmenu` on a canvas at all, so without this there is no way to
  // reach the menu on the device this port exists for.
  let press = null;
  const cancelPress = () => {
    if (!press) return;
    win.clearTimeout(press.timer);
    press = null;
  };

  /**
   * Takes the finger away from the viewer, without it counting as a tap.
   *
   * ⚠ **Without this, a long press on a link card opens the card's page over the menu it just
   * opened.** `input.rs` decides a tap at `pointerup`, from how far the contact travelled —
   * and a finger resting still for half a second has travelled nothing, so it passes the slop
   * test perfectly. The user's finger lifts, wasm sees a clean tap on a card, and the browser
   * navigates away. One gesture, two answers.
   *
   * A synthetic `pointercancel` is the right instrument rather than a hack: `input.rs` split
   * `pointerup` and `pointercancel` into two handlers *specifically* so that an interruption
   * drops the contact without being mistaken for a deliberate tap — its own comment says
   * *"a release can open a link card's page; a cancel must never do that."* This is exactly
   * the interruption it means, and the long press is what interrupted it.
   */
  const releaseToViewer = (pointerId) => {
    const Event = win.PointerEvent;
    if (typeof Event !== 'function') return;
    canvas.dispatchEvent(new Event('pointercancel', {
      pointerId,
      pointerType: 'touch',
      bubbles: true,
      cancelable: true,
    }));
  };

  const onPointerDown = (event) => {
    if (event.pointerType === 'mouse') return;
    // A second finger is a pinch, not a press.
    if (press) { cancelPress(); return; }
    const at = { x: event.clientX, y: event.clientY };
    const pointerId = event.pointerId;
    press = {
      at,
      timer: win.setTimeout(() => {
        press = null;
        state.pressedRecently = Date.now();
        // Before the menu, so the board is settled by the time it draws.
        releaseToViewer(pointerId);
        openMenu(at.x, at.y);
      }, PRESS_MS),
    };
  };
  const onPointerMove = (event) => {
    if (!press) return;
    if (Math.hypot(event.clientX - press.at.x, event.clientY - press.at.y) > PRESS_SLOP) {
      cancelPress();
    }
  };

  // Outside a surface closes it.
  //
  // **Both `pointerdown` and `click`, and neither is redundant.** `pointerdown` is what makes
  // a menu go away the moment the next gesture *begins* rather than when it ends — a menu
  // that survives the start of a pan is a menu the board is moving underneath. `click` is
  // the backstop for a press that never arrives here: this file itself stops a right-button
  // `pointerdown` at the window, and `input.rs` takes a pointer capture on the canvas, so a
  // closer with only the one listener has gestures it cannot see.
  //
  // ⚠ **And `click` is exactly what makes the fresh guard load-bearing.** The `⋮` on the
  // selection bar opens this menu from its own `click` handler, and that same click then
  // bubbles to the document in the same tick — where the closer finds a target outside a
  // rectangle that did not exist when the press began, and closes the menu it just opened.
  // The menu opens and closes in one event and is never seen. That is `Opened::fresh` in
  // `context_menu.rs`, arrived at by the same route: *"it cost a round of the right button
  // does nothing."*
  const onOutside = (event) => {
    if (state.guard) return;
    const target = event.target;
    if (state.menuOpen && !menu.contains(target)) closeMenu();
    if (state.popOpen && !pop.contains(target) && target !== state.popOpen
      && !(state.popOpen.contains && state.popOpen.contains(target))) {
      closePopover();
    }
  };

  // ⚠ **Capture, and `stopPropagation` only while something is open.** `chrome.js` binds
  // Escape to *leave the board*, and it registered first — so an Escape meant to close this
  // menu would navigate the reader off the page they are editing. Taking the key
  // unconditionally is the other half of the same mistake: Escape on a board with nothing
  // open is still the way out.
  const onKeyDown = (event) => {
    if (event.key === 'Escape' && (state.menuOpen || state.popOpen)) {
      event.stopPropagation();
      event.preventDefault();
      closeMenu();
      closePopover();
      return;
    }
    if (!can('set_tool')) return;
    // ⌘/⌃/⌥ chords belong to the browser and to whatever binds them later; a bare letter is
    // a different binding, exactly as the desktop's bare `V`, `N` and `T` are.
    if (event.metaKey || event.ctrlKey || event.altKey) return;
    // ⚠ The line that stops a tool being armed by the letters of a word. `CLAUDE.md`
    // feedback 40 is this exact bug on the desktop: typing on a board ran the shortcut table,
    // so `n` armed the sticky tool mid-word and Backspace deleted the frame being renamed.
    // There is no on-canvas caret in the browser build yet; this is here so that the day
    // there is, the guard is already the one the desktop arrived at.
    const target = event.target;
    if (target && target.isContentEditable) return;
    if (target && /^(INPUT|TEXTAREA|SELECT)$/.test(target.tagName || '')) return;
    const key = String(event.key || '').toLowerCase();
    for (const group of TOOL_GROUPS) {
      for (const tool of group) {
        if (tool.key === key) {
          event.preventDefault();
          armTool(tool.id);
          return;
        }
      }
    }
  };

  win.addEventListener('pointerdown', onCapturePointerDown, true);
  win.addEventListener('keydown', onKeyDown, true);
  doc.addEventListener('pointerdown', onOutside);
  doc.addEventListener('click', onOutside);
  canvas.addEventListener('contextmenu', onContextMenu);
  canvas.addEventListener('pointerdown', onPointerDown);
  canvas.addEventListener('pointermove', onPointerMove);
  canvas.addEventListener('pointerup', cancelPress);
  canvas.addEventListener('pointercancel', cancelPress);
  // Our own surfaces get no browser menu either — a right-click on a Velm control raising
  // *Reload* and *View Source* is the tell that the control is not really part of the app.
  for (const surface of [palette, undo, bar, menu, pop]) {
    surface.addEventListener('contextmenu', (event) => event.preventDefault());
  }

  // ------------------------------------------------------------------ the caret's keyboard

  // ⚠ **A hidden focused `<textarea>`, not a bare `keydown` on the window.**
  //
  // A `<canvas>` cannot take text input, so on a tablet — which is why this client exists —
  // a keydown listener gives a board you cannot type into at all: **iOS raises the software
  // keyboard only for a focused editable element.** So one exists, offscreen, and the caret
  // reads its events.
  //
  // Offscreen rather than hidden, and the distinction is the whole trick: `display: none` and
  // `visibility: hidden` cannot take focus, so either of them puts the keyboard back where it
  // was. A 1×1 element at `left: -9999px` is focusable and invisible.
  //
  // What this costs, stated rather than discovered later: an IME composes into the field and
  // will double-insert, because nothing here listens for `compositionstart`. The desktop's
  // IME is itself listed as *wired and unverified*; this is a known gap, not a claim.
  const field = doc.createElement('textarea');
  field.setAttribute('aria-hidden', 'true');
  field.tabIndex = -1;
  field.autocapitalize = 'off';
  field.autocomplete = 'off';
  field.spellcheck = false;
  field.style.cssText =
    'position:fixed;left:-9999px;top:0;width:1px;height:1px;opacity:0;padding:0;border:0';

  const caretOpen = () => Boolean(can('velm_caret_open_at'));
  const commitCaret = () => {
    if (can('velm_caret_commit')) invoke('velm_caret_commit');
  };

  const onDoubleClick = (event) => {
    if (!caretOpen()) return;
    if (invoke('velm_caret_open_at', event.clientX, event.clientY) === true) {
      // Cleared before focusing, or the field accumulates every character typed this session
      // and a paste into it would carry the lot.
      field.value = '';
      field.focus({ preventScroll: true });
    }
  };
  const onFieldKeyDown = (event) => {
    field.value = '';
    const answer = invoke('velm_caret_key', event.key, event.metaKey, event.ctrlKey,
      event.shiftKey, event.altKey);
    // 0 — not ours, let the board's bindings have it. 1 — handled, stop here. 2 — the
    // *browser* has to finish it: a clipboard chord, whose `copy`/`cut`/`paste` events are
    // only produced if the keydown is left alone. Preventing 2 is how a paste stops arriving,
    // which is trap 9 in the other direction and worth the three-valued answer.
    if (answer === 1) event.preventDefault();
  };
  const onFieldCopy = (event) => {
    if (!event.clipboardData) return;
    event.clipboardData.setData('text/plain', invoke('velm_caret_selection') || '');
    event.preventDefault();
  };
  const onFieldCut = (event) => {
    if (!event.clipboardData) return;
    event.clipboardData.setData('text/plain', invoke('velm_caret_cut') || '');
    event.preventDefault();
  };
  const onFieldPaste = (event) => {
    if (!event.clipboardData) return;
    invoke('velm_caret_insert', event.clipboardData.getData('text/plain'));
    event.preventDefault();
  };
  // ⚠ Both of these are the same rule as the desktop's tab switch and its quit path: **give
  // every way a gesture can end without a release a call to the thing that closes it.** A
  // session left open holds an undo group open, and the next grouped operation on the board
  // fails — for the rest of the session, on every later move, delete and restyle.
  const onFieldBlur = () => commitCaret();
  const onHidden = () => {
    if (doc.hidden) commitCaret();
  };

  // ⚠ **A finger's way in, and without it the caret is unreachable on the one device this
  // client exists for.** `dblclick` is a mouse event: iOS synthesises it only after its own
  // 300ms double-tap-to-zoom heuristic, and not at all on a canvas that has claimed the
  // pointer. So a double *tap* is detected here, from the pointer stream that is already
  // being listened to.
  //
  // The window is 320ms and the slop 24 physical pixels — both deliberately looser than a
  // mouse's, because a finger lands somewhere slightly different each time and a second tap
  // that misses is not a second tap, it is a deselect. `pointerType === 'touch'` gates it:
  // letting a mouse through would give one gesture two openings and the second would fight
  // the first.
  let lastTap = null;
  const onTapForCaret = (event) => {
    if (event.pointerType !== 'touch' || !caretOpen()) return;
    const now = event.timeStamp;
    const near = lastTap
      && now - lastTap.t < 320
      && Math.hypot(event.clientX - lastTap.x, event.clientY - lastTap.y) < 24;
    lastTap = near ? null : { t: now, x: event.clientX, y: event.clientY };
    if (!near) return;
    if (invoke('velm_caret_open_at', event.clientX, event.clientY) === true) {
      field.value = '';
      // ⚠ `preventScroll`, or iOS scrolls the whole page to bring an offscreen element into
      // view — which moves the canvas out from under the board the caret just opened in.
      field.focus({ preventScroll: true });
    }
  };
  canvas.addEventListener('pointerup', onTapForCaret);

  canvas.addEventListener('dblclick', onDoubleClick);
  field.addEventListener('keydown', onFieldKeyDown);
  field.addEventListener('copy', onFieldCopy);
  field.addEventListener('cut', onFieldCut);
  field.addEventListener('paste', onFieldPaste);
  field.addEventListener('blur', onFieldBlur);
  doc.addEventListener('visibilitychange', onHidden);

  // ------------------------------------------------------------------ mounting

  // After the canvas, like `chrome.js`, so a positioned element paints above the static
  // canvas on DOM order alone and the page's own fixed panels — the status line, the board
  // list — still paint above these.
  canvas.insertAdjacentElement('afterend', field);
  field.insertAdjacentElement('afterend', palette);
  palette.insertAdjacentElement('afterend', undo);
  undo.insertAdjacentElement('afterend', bar);
  bar.insertAdjacentElement('afterend', menu);
  menu.insertAdjacentElement('afterend', pop);

  // The undo pair sits under the column. Measured from the palette rather than positioned by
  // a constant, because the column's height is the number of tools times the touch target
  // and neither is fixed here.
  const placeUndo = () => {
    const box = palette.getBoundingClientRect();
    if (!box.height) return;
    const own = undo.getBoundingClientRect().height || 96;
    // Clamped to the window, or a tall palette on a short screen pushes the history pair off
    // the bottom edge — where it is not merely hard to reach but absent.
    const y = Math.min(box.bottom + GAP, Math.max(GAP, (win.innerHeight || 0) - own - GAP));
    const next = `${Math.round(y)}px`;
    if (undo.style.top !== next) undo.style.top = next;
  };
  placeUndo();
  refreshHistory();

  // See the `editing` note above. Only when the build has the switch at all — an older wasm
  // simply has no such mode, and calling nothing is the honest thing to do about that.
  const throwsSwitch = editing && Boolean(can('set_editing'));
  if (throwsSwitch) invoke('set_editing', true);

  const session = {
    palette,
    undo,
    bar,
    menu,
    pop,
    refresh,
    /** The armed tool, for a fixture. */
    tool: () => state.tool,
    unmount() {
      if (!session.live) return;
      session.live = false;
      state.live = false;
      cancelPress();
      // Symmetric with the mount. A board left in editing mode with no palette, no menu and
      // no selection bar is a board whose one-finger drag has quietly stopped panning and
      // has nothing on screen to say why.
      if (throwsSwitch) invoke('set_editing', false);
      win.removeEventListener('pointerdown', onCapturePointerDown, true);
      win.removeEventListener('keydown', onKeyDown, true);
      doc.removeEventListener('pointerdown', onOutside);
      doc.removeEventListener('click', onOutside);
      canvas.removeEventListener('contextmenu', onContextMenu);
      canvas.removeEventListener('pointerdown', onPointerDown);
      canvas.removeEventListener('pointermove', onPointerMove);
      canvas.removeEventListener('pointerup', cancelPress);
      canvas.removeEventListener('pointercancel', cancelPress);
      // The text session goes down before its keyboard does, or it holds an undo group open
      // on a board the palette has stopped being able to reach.
      commitCaret();
      canvas.removeEventListener('pointerup', onTapForCaret);
      canvas.removeEventListener('dblclick', onDoubleClick);
      field.removeEventListener('keydown', onFieldKeyDown);
      field.removeEventListener('copy', onFieldCopy);
      field.removeEventListener('cut', onFieldCut);
      field.removeEventListener('paste', onFieldPaste);
      field.removeEventListener('blur', onFieldBlur);
      doc.removeEventListener('visibilitychange', onHidden);
      field.remove();
      for (const surface of [palette, undo, bar, menu, pop]) surface.remove();
      if (mounted === session) mounted = null;
    },
  };
  session.live = true;
  mounted = session;

  // ⚠ **A rAF poll, and it wakes nothing that was asleep.** `schedule_frame` in `lib.rs`
  // chains `requestAnimationFrame` unconditionally for the life of the page, so this adds a
  // callback to a frame the browser was already going to run — the same argument `chrome.js`
  // makes for its zoom readout, and the reason the desktop app can refuse a blinking caret
  // and still afford a live readout here. It also stops on its own when the tab is hidden,
  // which is right: a selection bar on a tab nobody is looking at has nothing to follow.
  if (typeof win.requestAnimationFrame === 'function'
    && (can('selection_summary') || can('history_state'))) {
    const poll = () => {
      if (!session.live) return;
      // ⚠ A create tool **disarms itself** after one placement — every tool but the pen and
      // the eraser does — so the palette has to read what is armed rather than remember what
      // it last asked for. Without this the sticky button stays lit over a board that is back
      // on Select, which is the interface lying about what the next click will do.
      const armed = invoke('current_tool');
      if (typeof armed === 'string' && armed !== state.tool) showArmed(armed);
      refresh();
      placeUndo();
      win.requestAnimationFrame(poll);
    };
    win.requestAnimationFrame(poll);
  }
  // With no `selection_summary` the bar is not drawn at all, and that is the one place this
  // file prefers absence to a disabled control: there is nothing to derive its contents from
  // and no rectangle to float it above. The palette and the menu still draw, disabled, which
  // is where a reader finds out what this build cannot do.

  return session;
}
