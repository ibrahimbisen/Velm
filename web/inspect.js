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
// Colour
// ---------------------------------------------------------------------------------------

function clampByte(n) {
  const v = Math.round(Number(n));
  if (!Number.isFinite(v)) return 0;
  return Math.min(255, Math.max(0, v));
}

function hex2(n) {
  return clampByte(n).toString(16).padStart(2, '0');
}

/** `{r,g,b,a}` to the `#rrggbb` an `<input type="color">` takes. Alpha is not part of it. */
function toHex(color) {
  if (!color) return '#000000';
  return '#' + hex2(color.r) + hex2(color.g) + hex2(color.b);
}

/**
 * `#rrggbb` back to `{r,g,b,a}`, **carrying the alpha it had**.
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
function fromHex(hex, alpha) {
  const text = String(hex || '').replace('#', '');
  return {
    r: parseInt(text.slice(0, 2), 16) || 0,
    g: parseInt(text.slice(2, 4), 16) || 0,
    b: parseInt(text.slice(4, 6), 16) || 0,
    a: alpha === undefined || alpha === null ? 255 : clampByte(alpha),
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
