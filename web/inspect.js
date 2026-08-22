// The properties panel for the browser client — the desktop's docked inspector, in the DOM.
//
// `crates/vellum-app/src/inspect.rs` is the specification and `crates/vellum-ui/src/{selection,
// properties}.rs` are the layout. This file is the third implementation of the same idea and
// deliberately not a third *derivation* of it: the mixed-value fold, the headline, the
// bounding box and the `size_editable` rule are all decided once, in `PanelModel::derive`,
// and arrive here already folded. Re-deciding any of them in JavaScript would be two answers
// to one question, which is the failure `theme.rs` and `draw::kanban_runs` already exist to
// prevent.
//
// # ⚠ Every control is gated on the export behind it
//
// The browser client is a **viewer**. `docs/08-web.md` §1 says so in as many words —
// *"Browser editing is a later, separately-decided stage"* — and `vellum-web/src/input.rs`
// still opens with *"there is nothing here to select, drag or place."* So on the build this
// file was written against, **not one of the write verbs exists**, and neither does the
// selection they would act on.
//
// That is not a reason to ship a panel that throws. It is the reason every control asks
// `typeof mod.fn === 'function'` first and, when the answer is no, is **drawn disabled with a
// tooltip naming the export that is missing**. CLAUDE.md calls describing a gesture the user
// cannot perform the worst failure in its own list, and `inspect.rs` answers exactly this way
// for the controls `vellum_doc::Style` cannot take: `Applied::Unsupported(&'static str)`, a
// reason rather than a silent no-op. A disabled control with a reason is the house style; an
// absent one teaches nothing and an inert one lies.
//
// The one thing gated to *nothing* is the panel itself: with no `selection_summary` there is
// no selection to describe, so `mountInspector` returns `null` and does not even draw its
// toggle. A switch for a panel that can never fill is the same broken promise one level up.
//
// # What it reads, and what it writes
//
// `crates/vellum-web/src/style.rs` is the wire contract and this file is written against it
// rather than against a shape proposed here. Three exports, all optional at mount:
//
//   selection_summary()       -> the `Summary` JSON. `{}` means *could not answer* -- the
//                                board is not ready, or a frame holds the viewer borrowed --
//                                and is not the same as a selection of nothing, which
//                                carries `count: 0`.
//   style_selection(json)     -> "" when it landed, otherwise the sentence to show
//   transform_selection(json) -> the same
//
// Four readings of a property and the panel has to tell them apart, which is why `Prop`
// carries a `state` word instead of a bare value:
//
//   key absent                        nothing selected has it -- draw no control
//   {"state":"uniform","value":X}     they agree
//   {"state":"unset"}                 they agree and the document says nothing:
//                                     the board default for a fill, auto-fit for a size
//   {"state":"mixed"}                 they disagree -- placeholder, and an edit assigns to all
//
// A colour is `{"hex":"#rrggbb","alpha":0-255}`. An edit is the variant name as the single
// key: `{"fill":{"hex":"#ff0000","alpha":255}}`, `{"fill":null}` to clear, `{"locked":true}`,
// `{"position":{"x":100,"y":40}}`. `Summary::unsupported` names the controls to grey *before*
// they are pressed, which is one better than the desktop can do -- `inspect.rs` can only
// answer at `apply_style` time.
//
// # The panel is off by default, and it is not drawn for an empty selection
//
// `properties.rs`'s own header carries the user's verdict — *"i dont need this panels i dont
// even understand the purpose of it"* — and returns an empty rect the moment the selection is
// empty. Both halves are matched here: the toggle starts off, and with nothing selected the
// body holds one muted line rather than a column of controls.
//
// ⚠ **That one line is a deliberate deviation and it is the smallest one available.** The
// desktop's panel is reached from a menu, so a menu row that ticks is proof the command ran.
// Here the toggle *is* the whole affordance, and a toggle whose only visible effect is
// nothing is indistinguishable from a broken one. So pressing it with an empty selection says
// `Pick an object on the board` — `properties.rs`'s own `empty_state` string, at forty pixels
// rather than at two hundred and sixty-four.
//
// # It floats; it never takes the board's width
//
// `properties.rs` records the end state it was heading for: *"The proper end state is a
// floating panel over the canvas rather than a docked one that shrinks it. Then the canvas
// rectangle never changes, the camera never needs compensating."* A browser client gets that
// for free — the canvas is `100vw × 100vh` and this is `position: fixed` over it — so the
// camera compensation `properties.rs` warns the next implementer about is not needed and must
// not be added.
//
// It sits **top-right**: `chrome.js` owns the top-left, `#velm-status` owns the bottom-left,
// and the bottom edge is where a thumb rests on a tablet held either way up.
//
// # ⚠ Two things a DOM panel has that an immediate-mode one does not
//
// `properties.rs` rebuilds every control from the model sixty times a second and this cannot:
// a rebuild replaces the `<input>` the reader's fingers are in. Two guards, and both are the
// same bug seen from either end of a gesture.
//
//  - **The rebuild is deferred while focus is inside the panel.** The poll still *reads* the
//    summary every tick — that is what keeps the stamp below honest — and only the DOM write
//    waits. `properties.rs` carries the same note about its own text field: *"the field cannot
//    read straight from the model."*
//  - **A commit is stamped with what it was aimed at.** Committing on blur means the commit
//    can land *after* a tap has already moved the selection, which would write a number typed
//    for one item into another. The field records the selection's identity when it takes
//    focus and drops the edit if it has changed by the time it commits.
//
// # Colours are the page's, not `theme.rs`'s
//
// `#22282C`, `#656D73`, `#E2E7EA`, `#00A38C` — `index.html`'s and `chrome.js`'s own tokens.
// `chrome.js` states the reason and it holds here: this design draws structure with hairlines
// (`docs/05` §2), so two nearly-identical hairlines on one screen read as a mistake rather
// than as a system.

const STYLE_ID = 'velm-inspect-style';
const PANEL_CLASS = 'velm-inspect';

/// The desktop's `vellum_ui::theme::PROPERTIES_WIDTH`, to the pixel. Parity is worth having
/// here for a reason beyond tidiness: the two panels hold the same rows in the same order, so
/// a screenshot of one beside the other is a usable comparison — which `docs/08-web.md` §45
/// records as the instrument the whole port was missing.
const PANEL_WIDTH = 264;

/// How often the selection is re-read, in milliseconds.
///
/// ⚠ **A timer, not `requestAnimationFrame`, and the difference is not stylistic.** A
/// selection changes when a finger lands, which is a handful of times a minute; the camera
/// changes sixty times a second, which is why `chrome.js`'s zoom readout is on rAF and this
/// is not. Polling a JSON-serialising export at frame rate would spend a millisecond of every
/// frame building a string that is byte-identical to the last one — on the one platform whose
/// frame budget `docs/08-web.md` §8 already names as the weakest thing about this port.
///
/// 150ms is under the ~200ms at which a response stops feeling attached to the tap that
/// caused it, and it is 9 wasted string compares a second rather than 60.
const POLL_MS = 150;

/// The panel currently on the page, so a second `mountInspector` can take the first down.
let mounted = null;

/// Unique enough for `<label for>`, and stable across a rebuild is neither needed nor wanted.
let nextId = 0;

const CSS = `
.velm-inspect-toggle {
  position: fixed;
  top: max(12px, env(safe-area-inset-top));
  right: max(12px, env(safe-area-inset-right));
  display: inline-flex;
  align-items: center;
  justify-content: center;
  /* 44px, Apple's own minimum — the same number and the same reason as chrome.js's. */
  min-width: 44px;
  height: 44px;
  padding: 0;
  border-radius: 10px;
  border: 1px solid #E2E7EA;
  background: #fff;
  color: #22282C;
  box-shadow: 0 1px 2px rgba(26, 29, 31, .06), 0 2px 8px rgba(26, 29, 31, .08);
  appearance: none;
  -webkit-appearance: none;
  cursor: pointer;
  transition: background-color 120ms ease-out;
  -webkit-tap-highlight-color: transparent;
  touch-action: manipulation;
}
@media (hover: hover) {
  .velm-inspect-toggle:hover { background: #F2F5F6; }
}
.velm-inspect-toggle:active { background: #E9EEF0; }
.velm-inspect-toggle:focus-visible { outline: 2px solid #6FD6E6; outline-offset: -2px; }
/* Pressed is the accent, which is what it means everywhere else in Velm: *this is on*. */
.velm-inspect-toggle[aria-pressed='true'] { border-color: #00A38C; color: #00A38C; }
.velm-inspect-toggle svg { display: block; width: 20px; height: 20px; }

.velm-inspect {
  position: fixed;
  /* Clear of the toggle, which keeps its own corner. */
  top: calc(max(12px, env(safe-area-inset-top)) + 52px);
  right: max(12px, env(safe-area-inset-right));
  /* ⚠ Never wider than the window. On a phone a fixed 264 puts a quarter of the panel off
     the right edge, where there is no way to scroll to it — the board scrolls, not the page. */
  width: min(${PANEL_WIDTH}px, calc(100vw - 24px));
  max-height: calc(100vh - max(12px, env(safe-area-inset-top)) - env(safe-area-inset-bottom) - 76px);
  overflow-y: auto;
  /* ⚠ Stops a flick that reaches the end of the panel from carrying on into the page — which
     on iOS is the rubber-band index.html already turns off for the body, arriving by a
     second door. */
  overscroll-behavior: contain;
  box-sizing: border-box;
  padding: 10px 12px 12px;
  border-radius: 10px;
  border: 1px solid #E2E7EA;
  background: #fff;
  box-shadow: 0 1px 2px rgba(26, 29, 31, .06), 0 2px 8px rgba(26, 29, 31, .08);
  font: 13px/1.4 -apple-system, BlinkMacSystemFont, "Segoe UI", Inter, sans-serif;
  color: #22282C;
  -webkit-user-select: none;
  user-select: none;
}

.velm-inspect-head {
  display: flex;
  align-items: center;
  gap: 8px;
  padding-bottom: 8px;
  border-bottom: 1px solid #E2E7EA;
}
.velm-inspect-headline {
  flex: 1 1 auto;
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  font-weight: 600;
}

/* A section is a small muted heading and its rows. §2: a hairline where other designs use a
   shadow, and no boxes inside boxes. */
.velm-inspect-section {
  margin-top: 12px;
  font: 11px/1.4 -apple-system, BlinkMacSystemFont, "Segoe UI", Inter, sans-serif;
  font-weight: 600;
  letter-spacing: .04em;
  text-transform: uppercase;
  color: #8B959B;
}

.velm-inspect-row {
  display: flex;
  align-items: center;
  gap: 8px;
  /* 36 rather than 44: a *row* is not a hit target, its control is, and the controls below
     each carry their own minimum. Stacking sixteen 44px rows would make the panel taller than
     an iPad in landscape and put the position fields below the fold. */
  min-height: 36px;
}
.velm-inspect-label {
  flex: 1 1 auto;
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  color: #656D73;
}
.velm-inspect-control { flex: none; display: inline-flex; align-items: center; gap: 6px; }

/* *Mixed*, where a multi-selection disagrees. Two em dashes would be quieter and would also
   read as "nothing"; the word is what properties.rs puts here and it is the honest one. */
.velm-inspect-mixed {
  font: 11px/1.4 ui-monospace, SFMono-Regular, Menlo, monospace;
  color: #8B959B;
}

.velm-inspect-btn {
  appearance: none;
  -webkit-appearance: none;
  display: inline-flex;
  align-items: center;
  justify-content: center;
  gap: 4px;
  min-width: 32px;
  height: 32px;
  padding: 0 8px;
  margin: 0;
  border: 1px solid #E2E7EA;
  border-radius: 6px;
  background: #fff;
  color: inherit;
  font: inherit;
  cursor: pointer;
  transition: background-color 120ms ease-out;
  -webkit-tap-highlight-color: transparent;
  touch-action: manipulation;
}
@media (hover: hover) {
  .velm-inspect-btn:hover:not([disabled]) { background: #F2F5F6; }
}
.velm-inspect-btn:active:not([disabled]) { background: #E9EEF0; }
.velm-inspect-btn:focus-visible { outline: 2px solid #6FD6E6; outline-offset: -2px; }
.velm-inspect-btn[disabled] { color: #8B959B; border-color: #EEF1F3; cursor: default; }
.velm-inspect-btn[aria-pressed='true'] { border-color: #00A38C; color: #00A38C; }
.velm-inspect-btn svg { display: block; width: 18px; height: 18px; }

/* The lock, in the header, where properties.rs puts it. */
.velm-inspect-head .velm-inspect-btn { min-width: 32px; padding: 0 6px; }

/* Segmented: alignment's three buttons, joined into one control so they read as one choice. */
.velm-inspect-seg { display: inline-flex; }
.velm-inspect-seg .velm-inspect-btn { border-radius: 0; margin-left: -1px; min-width: 34px; padding: 0; }
.velm-inspect-seg .velm-inspect-btn:first-child { border-radius: 6px 0 0 6px; margin-left: 0; }
.velm-inspect-seg .velm-inspect-btn:last-child { border-radius: 0 6px 6px 0; }
/* The pressed one must paint its own left border over its neighbour's, or the accent edge is
   half-hidden by the -1px overlap above. */
.velm-inspect-seg .velm-inspect-btn[aria-pressed='true'] { position: relative; z-index: 1; }

.velm-inspect-num {
  box-sizing: border-box;
  width: 100%;
  height: 32px;
  padding: 0 6px;
  border: 1px solid #E2E7EA;
  border-radius: 6px;
  background: #fff;
  color: inherit;
  /* §3: monospace for anything numeric, tabular figures so a board coordinate's five digits
     do not make the field breathe as they change. */
  font: 12px/1.4 ui-monospace, SFMono-Regular, Menlo, monospace;
  font-variant-numeric: tabular-nums;
  -webkit-user-select: text;
  user-select: text;
}
.velm-inspect-num:focus-visible { outline: 2px solid #6FD6E6; outline-offset: -2px; }
.velm-inspect-num[disabled] { color: #8B959B; border-color: #EEF1F3; background: #FAFBFB; }
/* The spinners are 20px of hit target aimed at a value that changes by one world unit. A
   finger cannot use them and a mouse has the keyboard; the width they cost is the field's. */
.velm-inspect-num::-webkit-outer-spin-button,
.velm-inspect-num::-webkit-inner-spin-button { -webkit-appearance: none; margin: 0; }
.velm-inspect-num { -moz-appearance: textfield; }

.velm-inspect-grid {
  display: grid;
  grid-template-columns: 1fr 1fr;
  gap: 6px 8px;
  margin-top: 4px;
}
.velm-inspect-cell { display: flex; align-items: center; gap: 6px; }
.velm-inspect-cell label {
  flex: none;
  width: 1.6ch;
  color: #656D73;
  font: 12px/1.4 ui-monospace, SFMono-Regular, Menlo, monospace;
}

/* A colour is shown as the swatch itself — the whole content of the choice, which is the
   argument feedback 24 makes for the accent picker's rows. 32 rather than 44 for the same
   reason the rows are 36. */
.velm-inspect-swatch {
  appearance: none;
  -webkit-appearance: none;
  width: 44px;
  height: 32px;
  padding: 0;
  border: 1px solid #E2E7EA;
  border-radius: 6px;
  background: #fff;
  cursor: pointer;
  touch-action: manipulation;
}
.velm-inspect-swatch::-webkit-color-swatch-wrapper { padding: 2px; }
.velm-inspect-swatch::-webkit-color-swatch { border: 0; border-radius: 4px; }
.velm-inspect-swatch::-moz-color-swatch { border: 0; border-radius: 4px; }
.velm-inspect-swatch:focus-visible { outline: 2px solid #6FD6E6; outline-offset: -2px; }
.velm-inspect-swatch[disabled] { cursor: default; opacity: .5; }

.velm-inspect-range {
  width: 108px;
  /* The thumb is the target and it is already ~28px on every platform; the track is not. */
  height: 32px;
  touch-action: manipulation;
  accent-color: #00A38C;
}
.velm-inspect-range[disabled] { opacity: .5; cursor: default; }
.velm-inspect-readout {
  flex: none;
  min-width: 4ch;
  text-align: right;
  font: 12px/1.4 ui-monospace, SFMono-Regular, Menlo, monospace;
  font-variant-numeric: tabular-nums;
  color: #656D73;
}

.velm-inspect-select {
  max-width: 132px;
  height: 32px;
  padding: 0 4px;
  border: 1px solid #E2E7EA;
  border-radius: 6px;
  background: #fff;
  color: inherit;
  font: inherit;
  touch-action: manipulation;
}
.velm-inspect-select[disabled] { color: #8B959B; border-color: #EEF1F3; background: #FAFBFB; }
.velm-inspect-select:focus-visible { outline: 2px solid #6FD6E6; outline-offset: -2px; }

.velm-inspect-url {
  display: block;
  margin: 2px 0 6px;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  font: 12px/1.4 ui-monospace, SFMono-Regular, Menlo, monospace;
  color: #656D73;
  -webkit-user-select: text;
  user-select: text;
}

.velm-inspect-empty { padding: 6px 0 2px; color: #656D73; }

/* Why an edit did not land. inspect.rs answers Applied::Unsupported(reason) rather than
   dropping a control's edit, and a reason nobody is shown is a reason nobody has. */
.velm-inspect-note {
  margin-top: 10px;
  padding-top: 8px;
  border-top: 1px solid #E2E7EA;
  font-size: 12px;
  color: #A8620E;
}

@media (prefers-reduced-motion: reduce) {
  .velm-inspect-toggle, .velm-inspect-btn { transition: none; }
}
`;

// Icons: line drawings on a 20 grid, 1.5px stroke, square caps — `chrome.js`'s set and the
// same rule, so the two surfaces on this page are drawn by one hand. None is accented: in
// Velm the accent means *selection* or *the active thing*, which is what `aria-pressed`
// already colours below.
const ICON_PANEL = 'M3.5 4.5h13v11h-13zM12 4.5v11';
const ICON_LOCK = 'M6.5 9V6.75a3.5 3.5 0 017 0V9M4.5 9h11v6.5h-11z';
const ICON_UNLOCK = 'M6.5 9V6.75a3.5 3.5 0 016.9-.8M4.5 9h11v6.5h-11z';
const ICON_OPEN = 'M10.5 4.5H15.5V9.5M15.5 4.5L9 11M13 11.5v4h-9v-9h4';
const ICON_COPY = 'M7.5 7.5h8v8h-8zM12.5 7.5v-3h-8v8h3';
const ICON_ALIGN_LEFT = 'M4 5.5h12M4 10h8M4 14.5h11';
const ICON_ALIGN_CENTER = 'M4 5.5h12M6 10h8M4.5 14.5h11';
const ICON_ALIGN_RIGHT = 'M4 5.5h12M8 10h8M5 14.5h11';

/**
 * One icon, as a self-contained SVG string.
 *
 * `innerHTML` is used for exactly this and for nothing else in the file: the argument is one
 * of the module-level constants above, never a value that has been anywhere near a board.
 * Everything a document supplies — a headline, a URL, a family name, a reason — goes in
 * through `textContent`.
 */
function svg(path) {
  return '<svg viewBox="0 0 20 20" width="20" height="20" fill="none" aria-hidden="true">'
    + '<path d="' + path + '" stroke="currentColor" stroke-width="1.5"'
    + ' stroke-linecap="square" stroke-linejoin="miter"/></svg>';
}

function ensureStyle() {
  if (document.getElementById(STYLE_ID)) return;
  const style = document.createElement('style');
  style.id = STYLE_ID;
  style.textContent = CSS;
  document.head.append(style);
}

function el(tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined) node.textContent = text;
  return node;
}

// ---------------------------------------------------------------------------------------
// Reading a Field
//
// `vellum_ui::Field<T>` is three states and the encoding keeps all three, because two of
// them are only distinguishable by name. `fill` is the case that decides the shape:
// `Uniform(None)` is **no fill** — a choice a shape can make and a sticky cannot — and
// `Absent` is *nothing here has a fill at all*. Any encoding that spells both `null` folds a
// visible control into a missing one, which is how a shape loses the ability to have no fill.
// ---------------------------------------------------------------------------------------

/** Whether nothing in the selection carries the property. The control is not drawn. */
function isAbsent(field) {
  return !field || field.state === 'absent' || field.state === undefined;
}

/** Whether the selection disagrees. The control shows a placeholder and an edit assigns to all. */
function isMixed(field) {
  return Boolean(field) && field.state === 'mixed';
}

/**
 * Whether the selection agrees and the document says nothing.
 *
 * ⚠ **The state that is easiest to lose, and losing it costs a control.** `style.rs` spells
 * out what it means per property: for a fill it is *the board's default*, which `Style::fill`
 * keeps distinct from a transparent colour on purpose; for a font size it is **auto-fit** —
 * Miro's `fs: 0`, which is what every sticky on the reference board carries. Folding it into
 * absent hides the only control that can turn auto-fit off; folding it into uniform invents a
 * value the document does not hold.
 */
function isUnset(field) {
  return Boolean(field) && field.state === 'unset';
}

/**
 * The uniform value, or `fallback` when the field is mixed or absent.
 *
 * `vellum_ui::Field::or`, verbatim, including the half that matters: **editing from that
 * fallback still assigns to the whole selection**, which is Miro's behaviour and the reason
 * a mixed control is drawn live rather than disabled.
 */
function valueOr(field, fallback) {
  if (!field || field.state !== 'uniform') return fallback;
  return field.value === undefined ? fallback : field.value;
}

// ---------------------------------------------------------------------------------------
// The headline
//
// `Summary` carries `kinds` — `ItemKind::tag` values — rather than a composed sentence, and
// `style.rs` gives the reason: pluralising English in Rust so that a panel which also has to
// print the word *objects* can use it is work that produces a worse result in both places.
// The nouns are `vellum_ui::ItemFacet::noun`'s, so the two panels name a thing alike.
// ---------------------------------------------------------------------------------------

const NOUNS = {
  sticky: 'Sticky note',
  text: 'Text',
  ink: 'Drawing',
  image: 'Image',
  link_preview: 'Link',
  embed: 'Link',
  connector: 'Connector',
  frame: 'Frame',
  shape: 'Shape',
  table: 'Table',
  chart: 'Chart',
  mindmap: 'Mind map',
  kanban: 'Kanban board',
  group: 'Group',
  document: 'Document',
  agent: 'Agent',
  file_tree: 'File tree',
  agent_note: 'Note',
  browser: 'Browser',
};

/** English plurals for the nouns above. None is irregular, so a suffix rule is enough. */
function plural(noun) {
  return /(s|ch|x)$/.test(noun) ? noun + 'es' : noun + 's';
}

function headlineOf(model) {
  const count = model.count || 0;
  if (!count) return 'Nothing selected';
  const kinds = Array.isArray(model.kinds) ? model.kinds : [];
  // An unknown tag falls back to the tag itself rather than to "object": a board holding a
  // kind this build has never heard of should say what it is, not refuse to name it.
  const nouns = kinds.map((tag) => NOUNS[tag] || String(tag));
  if (count === 1) return nouns[0] || 'Object';
  if (nouns.length === 1) return count + ' ' + plural(nouns[0]).toLowerCase();
  return count + ' objects';
}

// ---------------------------------------------------------------------------------------
// Colour
// ---------------------------------------------------------------------------------------

function clampByte(n) {
  const v = Math.round(Number(n));
  if (!Number.isFinite(v)) return 0;
  return Math.min(255, Math.max(0, v));
}

/**
 * The wire's `{hex, alpha}` as the `#rrggbb` an `<input type="color">` takes.
 *
 * Three-digit shorthand is doubled, which is the CSS rule and what `Colour::to_doc` accepts
 * inbound. A colour the picker cannot show is black rather than a throw: this runs inside a
 * render, and a malformed byte on a board must not cost the reader the whole panel.
 */
function swatchHex(colour) {
  const digits = String((colour && colour.hex) || '').replace('#', '');
  if (!/^[0-9a-fA-F]+$/.test(digits)) return '#000000';
  if (digits.length === 3) {
    return '#' + digits[0] + digits[0] + digits[1] + digits[1] + digits[2] + digits[2];
  }
  if (digits.length >= 6) return '#' + digits.slice(0, 6).toLowerCase();
  return '#000000';
}

/**
 * `#rrggbb` back to the wire's `{hex, alpha}`, **carrying the alpha it had**.
 *
 * ⚠ `<input type="color">` has no alpha channel — it is the one thing it cannot express, and
 * a colour picked through it is opaque. Writing `a: 255` would silently make every
 * translucent fill on the board opaque the first time somebody nudged its hue, which is a
 * data-loss gesture disguised as a colour change. So the byte is taken from the value that
 * was there and the *transparency* stays where the user can see it: the Opacity row.
 *
 * This is the same split `context_bar` settled in feedback 41 from the other side — one
 * property, one control — rather than two alphas that disagree the moment either is used.
 */
function colourFrom(hex, alpha) {
  return {
    hex: swatchHex({ hex }),
    alpha: alpha === undefined || alpha === null ? 255 : clampByte(alpha),
  };
}

// ---------------------------------------------------------------------------------------
// Rows and controls
// ---------------------------------------------------------------------------------------

function section(parent, title) {
  parent.append(el('div', 'velm-inspect-section', title));
}

/** A labelled row. The label is a `<label>` so a tap on the words reaches the control. */
function row(parent, label, controlId) {
  const line = el('div', 'velm-inspect-row');
  const text = el('label', 'velm-inspect-label', label);
  if (controlId) text.htmlFor = controlId;
  const control = el('div', 'velm-inspect-control');
  line.append(text, control);
  parent.append(line);
  return control;
}

function mixedTag(parent) {
  // A word rather than a dash. `properties.rs` draws `mixed_placeholder` here for the same
  // reason: two dashes read as *nothing*, and nothing is what `Absent` means one row over.
  parent.append(el('span', 'velm-inspect-mixed', 'Mixed'));
}

/**
 * Disables a control and says why, in one place.
 *
 * ⚠ **This is the file's whole posture towards a missing export.** `inspect.rs` answers an
 * edit the document cannot take with `Applied::Unsupported(&'static str)` — a sentence, not
 * a silence — and `menu.rs` greys a command with the reason on its tooltip. The alternative,
 * drawing the control and letting the press throw, is the shape CLAUDE.md names as the worst
 * failure in its list: a gesture the user cannot perform, described as though they can.
 */
function refuse(node, why) {
  node.disabled = true;
  node.title = why;
  return node;
}

/** The sentence a control carries when the module has no verb behind it. */
function missing(fn) {
  return 'This build cannot change it: the viewer does not export ' + fn + '() yet';
}

function iconButton(label, hint, path) {
  const node = el('button', 'velm-inspect-btn');
  node.type = 'button';
  node.setAttribute('aria-label', label);
  node.title = hint;
  node.innerHTML = svg(path);
  return node;
}

/**
 * What a selection *is*, cheaply enough to compare.
 *
 * Used for one thing only: deciding whether the selection a field was typed into is still the
 * selection when that field commits. Committing on blur means the commit can land *after* a
 * tap has already moved the selection, and without this the number typed for one object would
 * be written into another.
 *
 * ⚠ **The summary carries no item id, so this is an approximation** — count, kinds, and where
 * the selection is. Two objects of the same kind at the same coordinates would read alike,
 * which on a board is the case where applying the edit to either is the same edit. It fails
 * *safe*: an unrelated change to the same numbers drops an edit and says so, where the
 * opposite mistake writes it somewhere nobody looked. **One `single_id: Option<String>` on
 * `Summary` would make it exact**, and the branch to use it is already here.
 */
function stampOf(model) {
  if (!model || !model.count) return 'none';
  if (model.single_id !== undefined && model.single_id !== null) return 'id:' + model.single_id;
  const at = (prop) => {
    if (!prop) return '';
    return prop.state === 'uniform' ? String(prop.value) : prop.state;
  };
  return [
    model.count,
    (Array.isArray(model.kinds) ? model.kinds : []).join(','),
    at(model.x), at(model.y), at(model.w), at(model.h),
  ].join('|');
}

/**
 * One decimal, and no trailing zero. `properties.rs` uses `max_decimals(1)`.
 *
 * ⚠ **An empty value stays empty, and that guard is the whole of it.** `Number('')` is `0`
 * and `0` is finite, so without the first line a field with no number to show — a Mixed X, a
 * Mixed border width, an auto-fitted font size — renders as `0`. That is not a blank field,
 * it is the panel *stating a value the item has not got*, and typing over it looks like a
 * correction rather than the first number anybody entered. Found by a fixture, not by
 * reading: every one of those paths passes `''` and every one of them drew a zero.
 */
function num1(value) {
  if (value === '' || value === null || value === undefined) return '';
  const n = Number(value);
  if (!Number.isFinite(n)) return '';
  return String(Math.round(n * 10) / 10);
}

/**
 * A typed number, in world units.
 *
 * ⚠ **It commits on blur and on Enter, and on nothing else.** An `input` listener would make
 * typing `100` pass through `1` and then `10` — three edits, two of which move the item
 * somewhere the user never asked for, and on a document with undo that is three undo steps
 * for one number. `properties.rs` gets this for free from `DragValue`, which only reports
 * `changed()` when a drag or an edit finishes; a DOM field has to be told.
 *
 * Escape puts the field back to what it was showing and gives up focus, which is what Escape
 * does everywhere else in Velm — `actions.rs`'s Escape ladder is *undo less than the rung
 * above*, and abandoning a half-typed number is the smallest rung there is.
 */
function numberField(ctx, parent, spec) {
  const id = 'velm-i-' + (nextId += 1);
  const cell = el('div', 'velm-inspect-cell');
  const label = el('label', null, spec.label);
  label.htmlFor = id;
  const input = el('input', 'velm-inspect-num');
  input.type = 'number';
  input.id = id;
  // A numeric keyboard on a tablet rather than the full alphabet.
  //
  // ⚠ **It does not solve the minus sign, and saying so is the point.** iOS's decimal keypad
  // has no `-` key, and neither does its plain numeric one, so a *negative* board coordinate
  // cannot be typed on the tablet this port exists for. `inputMode: 'text'` would offer one
  // and would take the numeric keypad away from every other number in the panel, which is
  // the worse trade for a board whose coordinates are mostly positive. Recorded as a known
  // gap rather than fixed by guessing: the honest fix is a sign toggle beside the field, and
  // that is a decision about the layout rather than a line here.
  input.inputMode = 'decimal';
  input.step = 'any';
  input.setAttribute('aria-label', spec.aria || spec.label);

  let shown = spec.value;
  input.value = num1(shown);

  if (!spec.enabled) {
    refuse(input, spec.why);
  } else {
    // Taken at focus rather than read at commit: the poll keeps reading the summary while a
    // field is focused (only the *rebuild* waits), so by the time blur fires the model may
    // already describe a different selection. Comparing the two is what stops a number typed
    // for one item landing in another.
    let focusStamp = null;
    input.addEventListener('focus', () => {
      focusStamp = ctx.stampNow();
    });
    const revert = () => {
      input.value = num1(shown);
    };
    const commit = () => {
      const typed = input.value.trim();
      if (typed === '') return revert();
      const next = Number(typed);
      if (!Number.isFinite(next)) return revert();
      if (next === shown) return;
      if (focusStamp !== null && focusStamp !== ctx.stampNow()) {
        // The selection moved out from under the field between the keystroke and the commit.
        // Dropping the edit is the only safe answer: applying it would write a number aimed
        // at one object into whatever is selected now.
        ctx.note('The selection changed before that number was committed, so it was not applied.');
        return revert();
      }
      shown = next;
      spec.commit(next);
    };
    input.addEventListener('blur', commit);
    input.addEventListener('keydown', (event) => {
      if (event.key === 'Enter') {
        // Not followed by a blur: someone filling in X and then Y wants the second field,
        // and taking focus away costs them the tab that gets there.
        event.preventDefault();
        commit();
      } else if (event.key === 'Escape') {
        event.preventDefault();
        revert();
        input.blur();
      }
      // ⚠ Everything else is deliberately left alone and **not** stopped from propagating.
      // `chrome.js`'s own key handler already returns early for an INPUT target, so `+`, `-`
      // and `0` typed here do not zoom the board. Swallowing keys as well would be a second
      // answer to a question that already has one.
    });
  }

  cell.append(label, input);
  parent.append(cell);
  return input;
}

/**
 * A slider that emits when it is let go.
 *
 * ⚠ The DOM already draws this line exactly where it needs to be: `input` fires on every
 * pixel of a drag and `change` fires when the pointer comes up. Listening to `input` would
 * be an edit per frame of the gesture — sixty CRDT writes and sixty undo steps for one
 * opacity — which is the failure `apply_link_fetches` and the sidecar's synchronous write
 * have each cost this project once already. The readout still follows the thumb, because a
 * slider with no number moving is a slider you cannot aim.
 */
function rangeControl(ctx, parent, spec) {
  const id = 'velm-i-' + (nextId += 1);
  const input = el('input', 'velm-inspect-range');
  input.type = 'range';
  input.id = id;
  input.min = String(spec.min);
  input.max = String(spec.max);
  input.step = String(spec.step === undefined ? 1 : spec.step);
  input.value = String(spec.value);
  input.setAttribute('aria-label', spec.aria || spec.label);
  const readout = el('span', 'velm-inspect-readout', spec.format(spec.value));

  if (!spec.enabled) {
    refuse(input, spec.why);
  } else {
    input.addEventListener('input', () => {
      readout.textContent = spec.format(Number(input.value));
    });
    input.addEventListener('change', () => {
      spec.commit(Number(input.value));
    });
  }

  parent.append(input, readout);
  return id;
}

/**
 * A colour swatch.
 *
 * Emits on `change`, never on `input`, for `rangeControl`'s reason: a native picker fires
 * `input` continuously while a finger is on the colour wheel.
 */
function colorControl(ctx, parent, spec) {
  const id = 'velm-i-' + (nextId += 1);
  const input = el('input', 'velm-inspect-swatch');
  input.type = 'color';
  input.id = id;
  input.value = swatchHex(spec.value);
  input.setAttribute('aria-label', spec.aria || spec.label);
  // Named on the control itself rather than left to be discovered: the swatch cannot show
  // alpha, so a reader who has just set a fill to 40% and sees an opaque square deserves the
  // sentence rather than the doubt.
  input.title = spec.why || 'Hue only. Transparency is the Opacity row.';

  if (!spec.enabled) {
    refuse(input, spec.why);
  } else {
    input.addEventListener('change', () => {
      spec.commit(colourFrom(input.value, spec.alpha));
    });
  }

  parent.append(input);
  return id;
}

/** Three joined buttons over one choice: alignment, and nothing else today. */
function segmented(ctx, parent, spec) {
  const group = el('div', 'velm-inspect-seg');
  group.setAttribute('role', 'group');
  group.setAttribute('aria-label', spec.aria);
  for (const option of spec.options) {
    const button = iconButton(option.label, option.label, option.icon);
    // `aria-pressed` rather than a class: it is what a screen reader reads and what the
    // stylesheet already colours, so the accent and the announcement cannot disagree.
    button.setAttribute('aria-pressed', String(option.value === spec.value));
    if (!spec.enabled) {
      refuse(button, spec.why);
    } else {
      button.addEventListener('click', () => spec.commit(option.value));
    }
    group.append(button);
  }
  parent.append(group);
  return group;
}

// ---------------------------------------------------------------------------------------
// The sections, in the desktop's order
//
// `properties.rs` draws header, Appearance, Typography, Connector, Link, agents, Arrange,
// Position and size. Connector, the agent family, Arrange, and the four typography controls
// the document cannot take (weight, vertical alignment, line height, dash) are **not built
// here** — see the module list in the report and `docs/08-web.md`. Nothing is stubbed: a row
// that is not built is not drawn, which is the honest state, and a row that is built is
// live or says why it is not.
// ---------------------------------------------------------------------------------------

const ALIGN_OPTIONS = [
  { value: 'left', label: 'Align left', icon: ICON_ALIGN_LEFT },
  { value: 'center', label: 'Align centre', icon: ICON_ALIGN_CENTER },
  { value: 'right', label: 'Align right', icon: ICON_ALIGN_RIGHT },
];

/**
 * The controls this selection may show greyed, and the sentence to put on each.
 *
 * ⚠ **The summary decides this, not the panel**, and that is worth more than it looks:
 * `style.rs` reports `unsupported` beside the properties themselves, so a control the
 * document cannot honour is grey **before** it is pressed rather than apologetic after. The
 * desktop can only answer at `apply_style` time, which is why `inspect.rs` has an
 * `Applied::Unsupported` at all. Here both are wired: this greys it in advance, and a
 * refusal that still comes back is shown in the note.
 */
function blockedControls(model) {
  const map = new Map();
  const list = Array.isArray(model.unsupported) ? model.unsupported : [];
  for (const entry of list) {
    if (entry && typeof entry.control === 'string') map.set(entry.control, String(entry.why || ''));
  }
  return map;
}

/** Whether a control may be pressed, and the sentence if not. */
function gate(ctx, control, verb) {
  const blocked = ctx.blocked.get(control);
  if (blocked) return { enabled: false, why: blocked };
  const has = verb === 'style' ? ctx.canStyle : ctx.canTransform;
  return has
    ? { enabled: true, why: null }
    : { enabled: false, why: missing(verb === 'style' ? 'style_selection' : 'transform_selection') };
}

/**
 * One colour row: the swatch, and what the three non-uniform readings say beside it.
 *
 * Shared by Fill and Border because the two differ in one word and one wire key, and a
 * second copy of this is a second place for `unset` to be got wrong.
 */
function colourRow(ctx, body, spec) {
  const prop = spec.prop;
  const control = row(body, spec.label);
  const can = gate(ctx, spec.control, 'style');
  const colour = prop.state === 'uniform' ? prop.value : null;

  colorControl(ctx, control, {
    label: spec.label,
    value: colour || { hex: '#ffffff' },
    alpha: colour ? colour.alpha : 255,
    enabled: can.enabled,
    why: can.why,
    commit: (next) => ctx.style({ [spec.control]: next }),
  });

  if (isMixed(prop)) {
    mixedTag(control);
  } else if (isUnset(prop)) {
    // The document says nothing, which for a fill or a stroke is *the board's default* --
    // `style.rs` is explicit that this is not the same as a transparent colour. Saying
    // "Default" is the only reading that does not invent a value.
    control.append(el('span', 'velm-inspect-mixed', 'Default'));
  } else {
    const clear = el('button', 'velm-inspect-btn', 'Default');
    clear.type = 'button';
    clear.title = 'Put it back to the board default';
    if (!can.enabled) {
      refuse(clear, can.why);
    } else {
      // `null` is the clear verb. What clearing *means* is decided per item by the Rust,
      // because it depends on where that kind keeps the property.
      clear.addEventListener('click', () => ctx.style({ [spec.control]: null }));
    }
    control.append(clear);
  }
}

function appearanceSection(ctx, body, model) {
  // A key that is not there is the fourth reading: nothing selected carries the property, so
  // no control is drawn. `'fill' in summary` is the whole test, which is why these are plain
  // truthiness checks and not a `Field` state comparison.
  if (!model.fill && !model.stroke && !model.stroke_width && !model.opacity) return;

  section(body, 'Appearance');

  if (model.fill) {
    colourRow(ctx, body, { label: 'Fill', control: 'fill', prop: model.fill });
  }
  if (model.stroke) {
    colourRow(ctx, body, { label: 'Border', control: 'stroke', prop: model.stroke });
  }

  if (model.stroke_width) {
    const control = row(body, 'Border width');
    const can = gate(ctx, 'stroke_width', 'style');
    numberField(ctx, control, {
      label: '',
      aria: 'Border width in world units',
      value: model.stroke_width.state === 'uniform' ? model.stroke_width.value : '',
      enabled: can.enabled,
      why: can.why,
      commit: (next) => ctx.style({ stroke_width: next }),
    });
    if (isMixed(model.stroke_width)) mixedTag(control);
  }

  if (model.opacity) {
    // Present only when the fill is not the whole item -- the sticky rule, decided in the
    // Rust: a note *is* its colour and its own swatch already carries the alpha, so a second
    // control beside it would be two ways to say one thing. Feedback 41 settled it there.
    const control = row(body, 'Opacity');
    const can = gate(ctx, 'opacity', 'style');
    const current = model.opacity.state === 'uniform' ? model.opacity.value : 1;
    rangeControl(ctx, control, {
      label: 'Opacity',
      aria: 'Opacity, percent',
      min: 0,
      max: 100,
      step: 1,
      value: Math.round(current * 100),
      format: (v) => v + '%',
      enabled: can.enabled,
      why: can.why,
      // Shown as a percentage and sent as a fraction: the wire is 0-1 and everything else in
      // this interface is percent.
      commit: (next) => ctx.style({ opacity: next / 100 }),
    });
    if (isMixed(model.opacity)) mixedTag(control);
  }
}

function textSection(ctx, body, model) {
  if (!model.text_color && !model.font_size && !model.align) return;

  section(body, 'Text');

  if (model.text_color) {
    const control = row(body, 'Colour');
    const can = gate(ctx, 'text_color', 'style');
    const colour = model.text_color.state === 'uniform' ? model.text_color.value : null;
    colorControl(ctx, control, {
      label: 'Text colour',
      value: colour || { hex: '#22282c' },
      alpha: colour ? colour.alpha : 255,
      enabled: can.enabled,
      why: can.why,
      commit: (next) => ctx.style({ text_color: next }),
    });
    if (isMixed(model.text_color)) mixedTag(control);
  }

  if (model.font_size) {
    const control = row(body, 'Size');
    const can = gate(ctx, 'font_size', 'style');
    // `unset` is auto-fit -- Miro's `fs: 0`, and what every sticky on the reference board
    // carries. An empty field with an Auto placeholder says that; standing a number in for it
    // would show a size the item has not got, and typing over that number would turn auto-fit
    // off without the user having asked.
    const auto = isUnset(model.font_size);
    const field = numberField(ctx, control, {
      label: '',
      aria: 'Font size in world units',
      value: model.font_size.state === 'uniform' ? model.font_size.value : '',
      enabled: can.enabled,
      why: can.why,
      commit: (next) => ctx.style({ font_size: next }),
    });
    if (auto) field.placeholder = 'Auto';
    if (isMixed(model.font_size)) {
      mixedTag(control);
    } else if (!auto) {
      const clear = el('button', 'velm-inspect-btn', 'Auto');
      clear.type = 'button';
      clear.title = 'Fit the text to its box, which is what a sticky does by default';
      if (!can.enabled) {
        refuse(clear, can.why);
      } else {
        clear.addEventListener('click', () => ctx.style({ font_size: null }));
      }
      control.append(clear);
    }
  }

  if (model.align) {
    const control = row(body, 'Alignment');
    const can = gate(ctx, 'align', 'style');
    segmented(ctx, control, {
      aria: 'Text alignment',
      // `Align::tag` on both sides -- "left", "center", "right" -- so the wire and the
      // document cannot come to spell it differently.
      value: model.align.state === 'uniform' ? model.align.value : null,
      options: ALIGN_OPTIONS,
      enabled: can.enabled,
      why: can.why,
      commit: (next) => ctx.style({ align: next }),
    });
    if (isMixed(model.align)) mixedTag(control);
  }

  // ⚠ **No font family row, and that is the summary's shape rather than an omission here.**
  // `style.rs`'s `Summary` carries no family and its `StyleEdit` has no variant for one, so a
  // picker would be a control with nothing behind it and no value to show in the meantime.
  // The desktop's panel is still the only place a family can be chosen.
}

/**
 * Whether a URL off a board may be handed to `window.open`.
 *
 * ⚠ **This is the XSS door that `textContent` does not cover.** A card's address comes out of
 * a document somebody else may have handled, and `window.open('javascript:...')` runs it in
 * this origin — which is the origin serving every board the user owns. `open_in_browser` on
 * the desktop filters the scheme for the same reason and its comment names the mechanism:
 * a document must not become a way to run things.
 *
 * **Copy is deliberately not filtered**, and the desktop's `copy_link` records why: that path
 * only ever writes characters, and a `mailto:` card's address is exactly what somebody
 * pressing Copy on it wants.
 */
function isOpenable(url) {
  return typeof url === 'string' && /^https?:/i.test(url.trim());
}

/**
 * A card's address, with Open and Copy.
 *
 * ⚠ **Built, tested, and drawing nothing today**, and it is named here rather than left to be
 * discovered: `style.rs`'s `Summary` has no `link_url` key, so this section is never reached
 * on the current build. It is one field away — a `link_url: Option<String>` filled for a
 * single-selection `LinkPreview` or `Embed`, exactly as `PanelModel::link_url` is — and the
 * two verbs need no Rust at all, because opening and copying are things a browser does.
 *
 * Kept rather than deleted because the alternative is worse in both directions: a card is
 * most of the reference board, and the tests below pin the scheme filter, which is the part
 * that must be right before it is ever switched on rather than after.
 */
function linkSection(ctx, body, model) {
  const url = model.link_url;
  if (typeof url !== 'string' || url === '') return;

  section(body, 'Link');
  // ⚠ `textContent`, and the reason is this line specifically: the string is a page address
  // out of a board file. As markup it is an injection; as text it is an address.
  body.append(el('span', 'velm-inspect-url', url));

  const control = el('div', 'velm-inspect-control');
  const line = el('div', 'velm-inspect-row');
  line.append(control);

  const open = iconButton('Open page', 'Open the page in a new tab', ICON_OPEN);
  if (!isOpenable(url)) {
    refuse(open, 'Only http and https pages can be opened from here');
  } else {
    open.addEventListener('click', () => {
      // `noopener` as well as `noreferrer`: without it the opened page gets a live
      // `window.opener` handle back into the board.
      window.open(url, '_blank', 'noopener,noreferrer');
    });
  }
  control.append(open);

  const copy = iconButton('Copy link', 'Copy the address', ICON_COPY);
  const clipboard = typeof navigator !== 'undefined' && navigator.clipboard;
  if (!clipboard) {
    // WebGPU already requires a secure context, so this should be unreachable in a working
    // client — which is exactly why it says so rather than throwing on the press.
    refuse(copy, 'This browser will not give the page a clipboard');
  } else {
    copy.addEventListener('click', () => {
      // A promise nobody awaits still rejects, and an unhandled rejection is a console line
      // on a page whose reader may be holding a tablet with no console.
      Promise.resolve(navigator.clipboard.writeText(url)).then(
        () => ctx.note(''),
        (error) => ctx.note('The address could not be copied: ' + String(error)),
      );
    });
  }
  control.append(copy);

  body.append(line);
}

function geometrySection(ctx, body, model) {
  if (!model.x && !model.y && !model.w && !model.h && !model.rotation && !model.bounds) return;

  section(body, 'Position and size');

  const place = gate(ctx, 'position', 'transform');
  const size = gate(ctx, 'size', 'transform');

  // ⚠ **X and Y travel together, because the wire verb is one message carrying both.**
  // `Transform::Position { x, y }` moves the selection so its minimum corner lands there, and
  // there is no verb for one axis — so committing X means sending the Y that is on screen
  // beside it. When either axis reads Mixed there is no honest number for the other half of
  // the pair, and the two fields are refused with the reason rather than sending a Y nobody
  // typed. **An `anchor: {x, y}` on the summary is what would restore typing a multi-selection
  // into place**; it is named in the report as the one field that would.
  const pairable = model.x && model.y
    && model.x.state === 'uniform' && model.y.state === 'uniform';
  const placeWhy = place.enabled
    ? (pairable ? null
      : 'These items are in different places, so there is no single X and Y to type. Drag '
        + 'them on the board instead.')
    : place.why;
  // `size_editable` is read, never re-derived from `count`. `style.rs` decides it — one
  // unlocked item — and states why: for several items the two fields describe a bounding box,
  // and there is no honest reading of "make this box 400 wide" that does not silently choose
  // between stretching the box and resizing every member of it.
  const sizeable = Boolean(model.size_editable) && model.w && model.h
    && model.w.state === 'uniform' && model.h.state === 'uniform';
  const sizeWhy = size.enabled
    ? (sizeable ? null
      : 'Several objects are selected, so one width and height cannot describe them.')
    : size.why;

  // ⚠ **Read from the *live* model, not from the one this render was built from**, and that
  // distinction is the whole of a bug that only appears when two fields are used in a row.
  //
  // Enter commits without blurring — deliberately, so X then Tab then Y is one gesture — so
  // focus never leaves the panel and the rebuild stays deferred for the whole run. The
  // captured `model` therefore still holds the X the reader has already changed, and pairing
  // Y's commit against it sends the **old** X back: the second edit silently reverts the
  // first. Asking the poll's latest copy instead is what makes a chain of edits accumulate
  // rather than fight.
  //
  // The fall-back to the drawn value is for the case where the live property is not uniform;
  // the commit stamp drops those commits anyway, so this only decides what a doomed message
  // would have said.
  const pair = (key) => {
    const live = ctx.live();
    const now = live && live[key];
    if (now && now.state === 'uniform') return now.value;
    const drawn = model[key];
    return drawn && drawn.state === 'uniform' ? drawn.value : 0;
  };
  const at = () => ({ x: pair('x'), y: pair('y') });
  const extent = () => ({ w: pair('w'), h: pair('h') });

  const grid = el('div', 'velm-inspect-grid');
  if (model.x) {
    numberField(ctx, grid, {
      label: 'X',
      aria: 'X position in world units',
      value: model.x.state === 'uniform' ? model.x.value : '',
      enabled: place.enabled && pairable,
      why: placeWhy,
      commit: (n) => ctx.transform({ position: { x: n, y: at().y } }),
    });
  }
  if (model.y) {
    numberField(ctx, grid, {
      label: 'Y',
      aria: 'Y position in world units',
      value: model.y.state === 'uniform' ? model.y.value : '',
      enabled: place.enabled && pairable,
      why: placeWhy,
      commit: (n) => ctx.transform({ position: { x: at().x, y: n } }),
    });
  }
  if (model.w) {
    numberField(ctx, grid, {
      label: 'W',
      aria: 'Width in world units',
      value: model.w.state === 'uniform' ? model.w.value : '',
      enabled: size.enabled && sizeable,
      why: sizeWhy,
      commit: (n) => ctx.transform({ size: { w: n, h: extent().h } }),
    });
  }
  if (model.h) {
    numberField(ctx, grid, {
      label: 'H',
      aria: 'Height in world units',
      value: model.h.state === 'uniform' ? model.h.value : '',
      enabled: size.enabled && sizeable,
      why: sizeWhy,
      commit: (n) => ctx.transform({ size: { w: extent().w, h: n } }),
    });
  }
  if (grid.children.length) body.append(grid);

  // ⚠ The read-only box, and it earns its row precisely when the four fields above cannot be
  // typed into. A multi-selection greys all four, and a section of four dead boxes reporting
  // nothing is worse than no section — this is the line that still answers *how big is this*.
  // Drawn only then, so a single item is not told its own size twice.
  if (model.bounds && !(pairable && sizeable)) {
    const control = row(body, 'Selection');
    const box = model.bounds;
    control.append(el('span', 'velm-inspect-readout', num1(box.w) + ' x ' + num1(box.h)));
  }

  if (model.rotation) {
    // Rotation is the one geometry verb that carries a single number, so it stays live for a
    // multi-selection where X, Y, W and H cannot.
    const control = row(body, 'Rotation');
    const can = gate(ctx, 'rotation', 'transform');
    numberField(ctx, control, {
      label: '',
      aria: 'Rotation in degrees, clockwise',
      value: model.rotation.state === 'uniform' ? model.rotation.value : '',
      enabled: can.enabled,
      why: can.why,
      commit: (n) => ctx.transform({ rotation: n }),
    });
    if (isMixed(model.rotation)) mixedTag(control);
  }
}

function header(ctx, body, model) {
  const head = el('div', 'velm-inspect-head');
  // ⚠ `textContent` on a string built from document data. `headlineOf` composes it from a
  // count and a list of kind tags, and an unknown tag is printed as itself — so a board can
  // put a string of its own choosing on this line, and it must land as text.
  head.append(el('span', 'velm-inspect-headline', headlineOf(model)));

  if (model.count) {
    // All of them, not some: a mixed lock is not locked, so the button offers Lock and
    // locking every one of them is the result. `style.rs` reaches locked items with this one
    // verb on purpose — it is the way out, and gating it on the lock would be a one-way door.
    const locked = model.locked && model.locked.state === 'uniform' && model.locked.value === true;
    const can = gate(ctx, 'locked', 'style');
    const button = iconButton(locked ? 'Unlock' : 'Lock', locked ? 'Unlock' : 'Lock',
      locked ? ICON_LOCK : ICON_UNLOCK);
    button.setAttribute('aria-pressed', String(locked));
    if (!can.enabled) {
      refuse(button, can.why);
    } else {
      button.addEventListener('click', () => ctx.style({ locked: !locked }));
    }
    head.append(button);
  }

  body.append(head);
}

function render(ctx, body, model) {
  // Cleared with `textContent`, which drops the children and every listener on them in one
  // statement. Anything that outlives a rebuild has to live outside this element, which is
  // why the note is a sibling rather than the last row.
  body.textContent = '';
  // Rebuilt per render rather than per control: `unsupported` describes this selection, and
  // asking the list nine times would be nine linear scans to draw one panel.
  ctx.blocked = blockedControls(model);
  header(ctx, body, model);

  if (!model.count) {
    // `properties.rs`'s own `empty_state` sentence. See the module header for why this line
    // exists at all when the desktop draws nothing: here the toggle is the only affordance,
    // and a toggle with no visible effect is indistinguishable from a broken one.
    body.append(el('div', 'velm-inspect-empty', 'Pick an object on the board'));
    return;
  }

  appearanceSection(ctx, body, model);
  textSection(ctx, body, model);
  linkSection(ctx, body, model);
  geometrySection(ctx, body, model);
}

// ---------------------------------------------------------------------------------------
// Mounting
// ---------------------------------------------------------------------------------------

/** What the panel shows before the viewer has answered, and whenever nothing is picked. */
const NOTHING = { count: 0, headline: 'Nothing selected' };

/**
 * What a write verb answered.
 *
 * ⚠ **The contract is an empty string, or the reason.** `lib.rs`'s `style_selection` and
 * `transform_selection` both answer `String`: `""` when the change landed *and* when it was a
 * legitimate no-op (`Ok(0)`), and otherwise a sentence meant to be put in front of the reader
 * — `Err(why)` from `apply_style`, `"the board is not ready"`, or `"that is not a style
 * change: …"` when the JSON this file produced did not parse.
 *
 * That last one is why a non-empty answer must never be swallowed: it is the only way a wire
 * shape this panel got wrong can be *seen* rather than felt as a control that quietly does
 * nothing. An earlier draft of this function ignored strings it could not classify, on the
 * reasoning that a module answering `"ok"` should not raise a false alarm. The reasoning was
 * sound and the contract is the opposite of what it assumed — which is the whole argument for
 * reading `lib.rs` rather than recalling it.
 *
 * A number and a `{"applied":…,"reason":…}` object are also understood, so that a future
 * wrapper changing shape degrades to silence rather than to a wrong sentence.
 */
function readReply(result) {
  if (result === null || result === undefined) return { reason: '' };
  if (typeof result === 'number') return { reason: '' };
  if (typeof result === 'object') {
    if (typeof result.applied === 'string') {
      return { reason: result.applied === 'unsupported' ? String(result.reason || '') : '' };
    }
    return { reason: '' };
  }
  const text = String(result).trim();
  // The success case, and the most common one. An early-out rather than a guard: the
  // fall-through at the bottom answers `{ reason: '' }` for an empty string too, which the
  // A/B confirmed by breaking this line and watching nothing go red. It is kept because it
  // is the branch taken on every successful edit and reading it first says so.
  if (text === '') return { reason: '' };
  if (/^-?\d+$/.test(text)) return { reason: '' };
  if (text.startsWith('{')) {
    try {
      const parsed = JSON.parse(text);
      if (parsed && typeof parsed === 'object' && typeof parsed.applied === 'string') {
        return { reason: parsed.applied === 'unsupported' ? String(parsed.reason || '') : '' };
      }
    } catch (error) {
      // Not JSON after all. It is still a sentence, and it falls through to being shown.
    }
  }
  return { reason: text };
}

/**
 * Take down whatever is mounted. Safe to call when nothing is.
 *
 * Exported because the page may want it — `index.html` hides the canvas to show the board
 * list, and a properties panel floating over a list of boards is chrome describing something
 * that is not on screen.
 */
export function unmountInspector() {
  if (!mounted) return;
  // ⚠ The flag first. The poll is a `setTimeout` chain, so one tick may already be scheduled
  // when the timer is cleared — `chrome.js` records the same trap, where a second mount left
  // the first one's timer writing into a span that had been removed from the document.
  mounted.live = false;
  if (mounted.timer) window.clearTimeout(mounted.timer);
  mounted.panel.remove();
  mounted.toggle.remove();
  mounted = null;
}

/**
 * Mount the properties panel and wire it to the wasm module.
 *
 * `mod` is the module namespace object — the same one `index.html` holds as `velm`. Which
 * controls are live is decided by which functions it actually exports, so this file can ship
 * before the write verbs do and every control it draws either works or says why it does not.
 *
 * Options:
 *   - `canvas`   the board's canvas, for insertion order. Required.
 *   - `open`     whether the panel starts open. **Defaults to false**, matching the desktop,
 *                where the panel is off until it is asked for.
 *
 * Returns the panel element, or `null` when there is no canvas or the module cannot describe
 * a selection at all — in which case nothing is drawn, not even the toggle. A switch for a
 * panel that can never fill is the same broken promise as a button that throws.
 */
export function mountInspector(mod, { canvas, open = false } = {}) {
  if (!mod || !canvas) return null;
  if (typeof mod.selection_summary !== 'function') return null;

  // ⚠ Idempotent, for `chrome.js`'s reason: a second call would otherwise stack a second
  // panel and a second poll, and two polls on one module is twice the work to show one thing.
  unmountInspector();
  ensureStyle();

  const canStyle = typeof mod.style_selection === 'function';
  const canTransform = typeof mod.transform_selection === 'function';

  const toggle = el('button', 'velm-inspect-toggle');
  toggle.type = 'button';
  toggle.setAttribute('aria-label', 'Properties');
  toggle.title = 'Properties — what is selected, and its size, colour and position';
  toggle.innerHTML = svg(ICON_PANEL);

  const panel = el('div', PANEL_CLASS);
  panel.setAttribute('role', 'region');
  panel.setAttribute('aria-label', 'Properties');
  const body = el('div', 'velm-inspect-body');
  const note = el('div', 'velm-inspect-note');
  // Announced: it appears only when something the reader just did did not happen, which is
  // exactly the class of change worth interrupting for. The selection readout is not
  // announced, for `chrome.js`'s zoom-readout reason — it changes constantly.
  note.setAttribute('aria-live', 'polite');
  note.hidden = true;
  panel.append(body, note);

  const session = {
    live: true,
    open: Boolean(open),
    toggle,
    panel,
    body,
    note,
    noteText: '',
    /// The summary string the model was parsed from. The comparison key, never re-parsed.
    raw: null,
    /// The summary string the DOM was *built* from. Behind `raw` while a field has focus,
    /// which is the whole of the deferred-rebuild rule.
    built: null,
    model: NOTHING,
    timer: null,
  };
  mounted = session;

  const setNote = (text) => {
    if (session.noteText === text) return;
    session.noteText = text;
    note.textContent = text;
    note.hidden = !text;
  };

  const focusInside = () => {
    const active = document.activeElement;
    return Boolean(active) && body.contains(active);
  };

  /**
   * Runs one write verb and reports what came back.
   *
   * Called by name rather than through a saved reference, so the module stays the receiver.
   * Every call is guarded: a throw here must become a sentence in the panel, not a dead page
   * — `panic = "abort"` poisons a wasm module, so the frame where the client is *most*
   * broken is the frame where an unguarded call would say the least.
   */
  const call = (name, argument) => {
    let result;
    try {
      result = mod[name](argument);
    } catch (error) {
      setNote('That edit could not be applied: ' + String(error));
      session.built = null;
      return;
    }
    const answer = readReply(result);
    // A reason nobody is shown is a reason nobody has.
    setNote(answer.reason);
    // ⚠ **Rebuilt after every write, refused or not.** `Ok(0)` — a legitimate no-op — answers
    // with the same empty string a successful edit does, so the panel cannot tell from the
    // reply whether the number it is showing is the number the document now holds. Forcing
    // the next tick to redraw from the summary is what makes the panel agree with the board
    // in both cases, and it costs one rebuild per edit rather than one per frame. The focus
    // guard still applies, so a field the reader is still in is not yanked out from under
    // them.
    session.built = null;
  };

  const ctx = {
    canStyle,
    canTransform,
    /// Which controls this selection may not use, and why. Replaced on every render from the
    /// summary's own `unsupported` list; a Map so a control asks once.
    blocked: new Map(),
    stampNow: () => stampOf(session.model),
    /// The latest summary the poll has read, which is **not** the one the DOM was built from
    /// whenever a field has focus. `geometrySection` needs it: see the note on `pair`.
    live: () => session.model,
    note: setNote,
    // Externally tagged, which is serde's default and what `StyleEdit`/`Transform` deserialise
    // from: `{"fill":{"hex":"#ff0000","alpha":255}}`, `{"fill":null}`, `{"locked":true}`,
    // `{"position":{"x":100,"y":40}}`. The panel writes the variant name as the single key.
    style: (edit) => {
      if (!canStyle) return;
      call('style_selection', JSON.stringify(edit));
    },
    transform: (edit) => {
      if (!canTransform) return;
      call('transform_selection', JSON.stringify(edit));
    },
  };

  // ⚠ **The lock rides on `style_selection`, and there is deliberately no second door.**
  // `StyleEdit::Locked` is the padlock in `style.rs`, and it is the one edit that reaches
  // locked items — because it is the way out. A separate `lock_selection` export would be a
  // second answer to one question, which is feedback 40's shape.

  const draw = () => {
    session.built = session.raw;
    render(ctx, body, session.model);
  };

  const setOpen = (next) => {
    session.open = Boolean(next);
    toggle.setAttribute('aria-pressed', String(session.open));
    panel.hidden = !session.open;
    // Rebuilt on the way in rather than kept warm while hidden: a hidden panel that keeps
    // re-rendering is the idle cost the Agent Canvas's own `is_empty()` early-out exists to
    // refuse, and the state it would be preserving is a form nobody is looking at.
    if (session.open) draw();
  };
  toggle.addEventListener('click', () => setOpen(!session.open));
  setOpen(session.open);

  const tick = () => {
    if (!session.live) return;
    let raw;
    try {
      raw = mod.selection_summary();
    } catch (error) {
      // ⚠ No reschedule. A wasm module that throws once throws for ever — `panic = "abort"`
      // poisons it — so asking again every 150ms for the life of the tab is noise on a page
      // that has already said everything it can. `chrome.js`'s sync tick does the same.
      setNote('The viewer stopped answering: ' + String(error));
      return;
    }

    let key = null;
    let model = null;
    if (raw === null || raw === undefined) {
      key = 'none';
      model = NOTHING;
    } else if (typeof raw === 'object') {
      // Tolerated for a module that hands back a real object rather than a string. The key
      // still has to be a string, because that is what the cheap comparison is.
      try {
        key = JSON.stringify(raw);
        model = raw;
      } catch (error) {
        key = null;
      }
    } else {
      const text = String(raw);
      if (text === 'busy') {
        // A frame holds the viewer borrowed. Keep the last reading rather than blanking it —
        // a panel that empties for one tick in the middle of a gesture is worse than one that
        // is 150ms stale, and it is the same rule `chrome.js` applies to the zoom readout.
        session.timer = window.setTimeout(tick, POLL_MS);
        return;
      }
      if (text === 'none' || text === '') {
        key = 'none';
        model = NOTHING;
      } else {
        try {
          const parsed = JSON.parse(text);
          if (parsed && typeof parsed === 'object') {
            key = text;
            model = parsed;
          }
        } catch (error) {
          key = null;
        }
      }
      // ⚠ **`{}` is not an empty selection.** It is `with_viewer`'s default, answered when
      // the board is not ready *or* when a frame already holds the viewer borrowed — so on a
      // busy frame this is what a poll gets. A real empty selection is `Summary::empty()`,
      // which carries `count: 0`; the missing key is the discriminator, and without it the
      // panel would blank itself for one tick every time a poll landed mid-render, which
      // reads as flicker rather than as an answer. Keeping the last reading is the same rule
      // `chrome.js` applies to the zoom readout, and the same one `busy` gets above.
      if (model && typeof model.count !== 'number') {
        session.timer = window.setTimeout(tick, POLL_MS);
        return;
      }
    }

    if (key !== null && key !== session.raw) {
      session.raw = key;
      session.model = model;
      // A new summary is a new situation, so a stale refusal goes with it.
      setNote('');
    }

    // ⚠ **The model is updated above whatever happens; only the DOM write waits.** That split
    // is what makes the commit stamp honest: a field that has been focused for ten seconds
    // still gets to ask what the selection is *now*, and drop an edit aimed at what it was.
    if (session.open && session.raw !== session.built && !focusInside()) draw();

    session.timer = window.setTimeout(tick, POLL_MS);
  };

  // ⚠ Scheduled, not called. `chrome.js` pays for this exact line: calling the first tick
  // here runs it before the panel is in the document, so a throw on that tick left no panel
  // at all — with `mounted` already set, so the retry removed something that was never
  // inserted.
  session.timer = window.setTimeout(tick, 0);

  // After the canvas and with no `z-index`: a positioned element paints above a static canvas
  // on DOM order alone, and `#velm-status` and `#velm-library`, declared later in the page,
  // then paint above this. That is the order worth having — the board list covers the panel
  // rather than the panel floating over a list of boards.
  canvas.insertAdjacentElement('afterend', panel);
  canvas.insertAdjacentElement('afterend', toggle);
  return panel;
}
