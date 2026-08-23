// Finding a word on the board, from a browser tab.
//
// `crates/vellum-web/src/find.rs` is a finished feature — an inverted index over every label
// on the board, two matching passes, a ranked total order, and a `focus_match` that puts a
// result on screen — and **nothing on the page called it.** Its own module note warns about
// exactly this: *"uncalled code is this repository's signature defect ... treat a search finds
// nothing report as a wiring problem in this file before suspecting the index."* This file is
// the wiring, and it is the first caller either of those exports has ever had.
//
// Read `chrome.js` first. This one deliberately repeats its shape — the `mountX(mod, options)`
// signature, the idempotent unmount, the container that is `pointer-events: none` with the
// interactive parts taking it back, the 44px touch targets, `:hover` behind `@media (hover:
// hover)`, icons drawn as geometry rather than typed as characters, and `textContent` in every
// place a string somebody else wrote reaches the DOM.
//
// # ⚠ `partial` is not decoration
//
// `find()` answers `{matches, indexed, partial}`. `partial` is `true` when the board is past
// `MAX_INDEXED_ITEMS` and only the first `indexed` items were indexed at all — so a caller
// that drops the flag is showing an incomplete answer as though it were complete, which is
// precisely what the cap exists to make *visible* rather than to hide. It is surfaced here as
// a **sentence naming the number**, not as a colour: `docs/05` §6 forbids colour as the only
// carrier of meaning, and "some of your board was not searched" is not a thing a tint can say.
//
// # ⚠ Nothing here is a gesture the viewer cannot answer
//
// Every control gates on the module actually exporting the function behind it, and the two
// exports are gated **separately** even though they land in the same crate — `chrome.js` does
// the same for `zoom_by` and `fit_board`, and for the same reason: a build with `find` and no
// `focus_match` can still list what is on the board, which is worth having, and its rows say
// in a tooltip which function is missing rather than doing nothing when pressed.
//
// The **keys** are gated by the same test. A build that cannot search must leave `⌘F` to the
// browser's own find bar: swallowing it and offering nothing is strictly worse than not
// binding it, because it takes away a working feature to replace it with a promise.
//
// # ⚠ Two collisions with the modules already on the page, both handled here
//
//   - **Escape.** `chrome.js` binds it to *leave the board*. An Escape meant to close this
//     panel must not navigate the reader off the page, so this listens in the **capture**
//     phase and calls `stopPropagation` **only while the panel is open** — `tools.js` reached
//     the same arrangement for its menus. It also stands down when `defaultPrevented` is
//     already set, which is what makes the two of them a ladder rather than a race: whichever
//     surface claimed the key first keeps it.
//   - **Placement.** `chrome.js` owns the top-left, `inspect.js`'s toggle the top-right, its
//     panel the right side, and `tools.js`'s palette the left. This sits **below the whole top
//     band**, measured from those surfaces' live `getBoundingClientRect()` and never from a
//     constant. A panel underneath another panel is a field you can type into and cannot see,
//     and the desktop shipped that once — a selection bar behind the tool column.
//
// # ⚠ Debounced, and the number is stated
//
// A search per character is fine on the reference board's 1,306 items and is not fine at the
// 20,000 the index is bounded at, where it is 20,000 documents scored per keystroke on the
// frame thread. `SEARCH_DEBOUNCE_MS` is 180: past a fast typist's ~120-160ms between
// characters, so a burst of typing collapses into one search, and under the ~200ms at which a
// response stops reading as immediate. Enter **flushes** it rather than waiting, so typing a
// word and pressing Enter straight away searches for the word that was typed and not for its
// first six letters.
//
// # Colours are the page's, not the theme's
//
// `#22282C`, `#656D73`, `#E2E7EA`, `#00A38C` are `index.html`'s and `chrome.js`'s own tokens,
// for the reason `chrome.js` gives: this design draws its structure with hairlines, and two
// nearly-identical hairlines on one screen read as a mistake rather than as a system. Light
// only, no `prefers-color-scheme` block — the page it lives in has none.

const STYLE_ID = 'velm-find-style';
const CONTAINER_CLASS = 'velm-find';

/// The gap between a floating surface and whatever it is keeping clear of. `tools.js`'s
/// number, because the two are measured against the same three surfaces and a find panel
/// eight pixels off a palette that a selection bar clears by six reads as a mistake.
const GAP = 8;

/// How long the field rests before a query is sent. See the header: 180ms is past the
/// interval between two characters of fast typing and under the point where a response stops
/// feeling immediate.
const SEARCH_DEBOUNCE_MS = 180;

/// The panel's preferred width, and the width below which it stops being usable.
///
/// The row is a field plus a count plus three 44px controls, so `MIN_PANEL_WIDTH` is the
/// point at which the field itself has nothing left. `place()` narrows the panel to whatever
/// span is free between the surfaces beside it and refuses to go under this — below it the
/// panel overlaps rather than shrinking, because a field two characters wide is not a
/// smaller version of this feature, it is a broken one.
const PANEL_WIDTH = 360;
const MIN_PANEL_WIDTH = 244;

/// The shortest the panel is ever squeezed vertically. Under this the list holds no rows and
/// the panel is a header with a scrollbar.
const MIN_PANEL_HEIGHT = 160;

/// A defensive ceiling on one excerpt.
///
/// `find.rs` windows an excerpt with `vellum_search::Snippet` and caps a label at
/// `MAX_LABEL_CHARS` (1,024 **characters**, its own note is emphatic about that), so this
/// should never bind. It is here because the string has crossed a JSON boundary from a
/// document somebody else wrote, and a row is cheaper to clip than a layout is to recover.
///
/// ⚠ Clipped by **code point**, via `Array.from`, never by `String.slice`. A JS string is
/// UTF-16 and `slice` will cut a surrogate pair in half — the same class of mistake as the
/// byte-slicing that aborted the desktop app on an Alibaba title written in Chinese.
const MAX_EXCERPT_CHARS = 300;

/// The surfaces currently on the page, so a second `mountFind` can take the first down.
///
/// ⚠ Idempotent for `chrome.js`'s reason and with its stakes: this file installs a window
/// keydown listener, a resize listener and a rAF poll, so a second mount without this leaves
/// the first one's `⌘F` opening a panel that has left the document.
let mounted = null;

const CSS = `
.velm-find {
  position: fixed;
  /* Overwritten by place() on the first pass, which runs synchronously at mount. These are
     what is drawn if it is ever skipped: below chrome.js's bar, out of everyone's way. */
  top: calc(max(12px, env(safe-area-inset-top)) + 60px);
  left: max(12px, env(safe-area-inset-left));
  display: flex;
  /* ⚠ The whole reason this can sit over the board at all. Taken back by the toggle, the
     panel and — explicitly, see below — the results list. */
  pointer-events: none;
  font: 13px/1.4 -apple-system, BlinkMacSystemFont, "Segoe UI", Inter, sans-serif;
  color: #22282C;
  -webkit-user-select: none;
  user-select: none;
}

.velm-find-toggle {
  pointer-events: auto;
  appearance: none;
  -webkit-appearance: none;
  display: inline-flex;
  align-items: center;
  justify-content: center;
  /* 44px, Apple's own minimum, and the number chrome.js and inspect.js both use. Without
     this control there is no find on a tablet at all: ⌘F needs a keyboard, and the device
     this port exists for has not got one. */
  min-width: 44px;
  height: 44px;
  padding: 0;
  border-radius: 10px;
  border: 1px solid #E2E7EA;
  background: #fff;
  color: #22282C;
  box-shadow: 0 1px 2px rgba(26, 29, 31, .06), 0 2px 8px rgba(26, 29, 31, .08);
  cursor: pointer;
  transition: background-color 120ms ease-out;
  -webkit-tap-highlight-color: transparent;
  touch-action: manipulation;
}
@media (hover: hover) {
  .velm-find-toggle:hover { background: #F2F5F6; }
}
.velm-find-toggle:active { background: #E9EEF0; }
.velm-find-toggle:focus-visible { outline: 2px solid #6FD6E6; outline-offset: -2px; }
.velm-find-toggle[disabled] { color: #8B959B; cursor: default; }
.velm-find-toggle[disabled]:hover { background: #fff; }
.velm-find-toggle svg { display: block; width: 20px; height: 20px; }

.velm-find-panel {
  pointer-events: auto;
  box-sizing: border-box;
  width: min(${PANEL_WIDTH}px, calc(100vw - 24px));
  /* place() writes the exact figures; this is the fallback if it never runs. */
  max-height: calc(100vh - 96px);
  /* ⚠ A column, so the list scrolls and the field does not. Scrolling the *panel* puts the
     field off the top of the screen on a long result list — which is the one control the
     reader needs while they are reading the list. */
  display: flex;
  flex-direction: column;
  border-radius: 10px;
  border: 1px solid #E2E7EA;
  background: #fff;
  box-shadow: 0 1px 2px rgba(26, 29, 31, .06), 0 2px 8px rgba(26, 29, 31, .08);
}
.velm-find-panel[hidden], .velm-find-toggle[hidden] { display: none; }

.velm-find-row {
  flex: none;
  display: flex;
  align-items: center;
  gap: 6px;
  padding: 4px;
}

.velm-find-input {
  flex: 1 1 auto;
  min-width: 0;
  box-sizing: border-box;
  height: 36px;
  padding: 0 8px;
  border: 1px solid #E2E7EA;
  border-radius: 6px;
  background: #fff;
  color: inherit;
  /* ⚠ **16px, and it is not a taste decision.** iOS zooms the whole page in when a field
     with a font under 16px takes focus, and this port's first device is an iPad: a find bar
     that magnifies the board every time it is used has taken the board away to show you a
     text box. The rest of this interface is 13px and this field is the exception. */
  font: 16px/1.4 -apple-system, BlinkMacSystemFont, "Segoe UI", Inter, sans-serif;
  /* The container turns selection off for the chrome around it; a field is text. */
  -webkit-user-select: text;
  user-select: text;
}
.velm-find-input:focus-visible { outline: 2px solid #6FD6E6; outline-offset: -2px; }
.velm-find-input::placeholder { color: #8B959B; }

.velm-find-count {
  flex: none;
  padding: 0 2px;
  /* §3: monospace and tabular figures for anything numeric, so "9 of 12" and "10 of 12" do
     not shuffle the buttons beside them. */
  font: 12px/1.4 ui-monospace, SFMono-Regular, Menlo, monospace;
  font-variant-numeric: tabular-nums;
  color: #656D73;
  white-space: nowrap;
}

.velm-find-btn {
  pointer-events: auto;
  appearance: none;
  -webkit-appearance: none;
  display: inline-flex;
  align-items: center;
  justify-content: center;
  min-width: 44px;
  height: 44px;
  /* ⚠ The controls never give. A narrow window has to take the difference out of something,
     and min-width 0 on the field makes the field the only candidate — without this the
     squeeze is shared and every button drops under the touch minimum at once. */
  flex: none;
  padding: 0;
  margin: 0;
  border: 0;
  border-radius: 6px;
  background: transparent;
  color: inherit;
  font: inherit;
  cursor: pointer;
  transition: background-color 120ms ease-out;
  -webkit-tap-highlight-color: transparent;
  touch-action: manipulation;
}
@media (hover: hover) {
  .velm-find-btn:hover { background: #F2F5F6; }
  .velm-find-btn[disabled]:hover { background: transparent; }
}
.velm-find-btn:active { background: #E9EEF0; }
.velm-find-btn:focus-visible { outline: 2px solid #6FD6E6; outline-offset: -2px; }
.velm-find-btn[disabled] { color: #8B959B; cursor: default; }
.velm-find-btn svg { display: block; width: 20px; height: 20px; }

/* ⚠ **The board was searched in part, and this is the sentence that says so.** Amber
   reinforces it; the words carry it. §6 is explicit that colour is never the only carrier of
   a meaning, and there is no tint that means "17,000 of your items were not looked at". */
.velm-find-partial {
  flex: none;
  margin: 0;
  padding: 0 10px 8px;
  font-size: 12px;
  line-height: 1.45;
  color: #A8620E;
}
/* A transient sentence: a refusal, or a module that stopped answering. Separate from the
   partial notice on purpose — one is a standing property of the board and the other is a
   thing that just happened, and a single element means each one silently deletes the other. */
.velm-find-note {
  flex: none;
  margin: 0;
  padding: 0 10px 8px;
  font-size: 12px;
  line-height: 1.45;
  color: #656D73;
}
.velm-find-partial[hidden], .velm-find-note[hidden] { display: none; }

.velm-find-list {
  /* ⚠ Explicitly, and this is the line the container's own pointer-events: none is written
     against: a list that cannot be touched cannot be scrolled, and a long result list on a
     tablet is exactly the case this feature is for. */
  pointer-events: auto;
  /* min-height: 0 is what lets a flex child shrink below its content and scroll. Without it
     the list grows the panel past its max-height and the page gets a scrollbar instead. */
  flex: 1 1 auto;
  min-height: 0;
  overflow-y: auto;
  /* Stops a flick that reaches the end of the list carrying on into the page — the same
     rubber-band index.html turns off for the body, arriving by a second door. */
  overscroll-behavior: contain;
  -webkit-overflow-scrolling: touch;
  border-top: 1px solid #E2E7EA;
}
.velm-find-list[hidden] { display: none; }

.velm-find-item {
  appearance: none;
  -webkit-appearance: none;
  display: block;
  box-sizing: border-box;
  width: 100%;
  /* A row is two lines of text and a finger has to land on it. */
  min-height: 44px;
  padding: 7px 10px;
  border: 0;
  border-bottom: 1px solid #F2F5F6;
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
  .velm-find-item:hover { background: #F2F5F6; }
  .velm-find-item[disabled]:hover { background: transparent; }
}
.velm-find-item:active { background: #E9EEF0; }
.velm-find-item:focus-visible { outline: 2px solid #6FD6E6; outline-offset: -2px; }
.velm-find-item[disabled] { cursor: default; }
/* The current match. The accent is what "you are here" means everywhere else in Velm — and
   the row is *also* the only one carrying aria-current, so a reader who cannot see the tint
   is told the same thing. */
.velm-find-item[aria-current='true'] { background: rgba(0, 163, 140, .12); }
@media (hover: hover) {
  .velm-find-item[aria-current='true']:hover { background: rgba(0, 163, 140, .18); }
}

.velm-find-where {
  display: block;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  font-size: 12px;
  color: #656D73;
}
.velm-find-excerpt {
  display: block;
  /* Two lines, then ellipsed. A four-line excerpt makes one row taller than the three rows
     under it and the list stops reading as a list. */
  display: -webkit-box;
  -webkit-line-clamp: 2;
  -webkit-box-orient: vertical;
  overflow: hidden;
  font-size: 13px;
  color: #22282C;
}
.velm-find-empty {
  padding: 10px;
  font-size: 12px;
  color: #656D73;
}

@media (prefers-reduced-motion: reduce) {
  .velm-find-toggle, .velm-find-btn, .velm-find-item { transition: none; }
}
`;

// Icons: line drawings on a 20 grid, 1.5px stroke, square caps — drawn as a set, exactly as
// `chrome.js` and `tools.js` draw theirs.
//
// ⚠ Geometry, never a character. `CLAUDE.md` trap 10: a glyph outside the bundled face draws
// tofu, and the desktop app has paid for this three times — `↗`, `▶` and `ⓘ` are all drawn
// there for the same reason. There is no `🔍` and no `×` in this file.
const ICON_FIND = 'M8.5 3.5a5 5 0 1 0 0 10 5 5 0 1 0 0-10M12.4 12.4 16.5 16.5';
const ICON_PREV = 'M5 12.5 10 7.5l5 5';
const ICON_NEXT = 'M5 7.5 10 12.5l5-5';
const ICON_CLOSE = 'M5.5 5.5 14.5 14.5M14.5 5.5 5.5 14.5';

/// The name each verb is exported under, in the order they are tried.
///
/// ⚠ One table rather than a spelling scattered through the file, which is `tools.js`'s own
/// arrangement and its reason: the editing exports landed as `velm_undo` where the contract
/// said `undo`, and guessing either way produces a build where every control is disabled and
/// the function behind it exists. `find` and `focus_match` are unprefixed in `lib.rs` today;
/// the prefixed spellings cost two array entries and remove the guess.
///
/// The **first** name is canonical — it is what a tooltip names when nothing matches, so a
/// reader is told which function to write rather than which of two spellings was tried.
const EXPORTS = {
  find: ['find', 'velm_find'],
  focus_match: ['focus_match', 'velm_focus_match'],
};

/** The name this module actually exports for a verb, or `null`. */
function bound(mod, name) {
  for (const candidate of EXPORTS[name] || [name]) {
    if (mod && typeof mod[candidate] === 'function') return candidate;
  }
  return null;
}

function svg(path) {
  return '<svg viewBox="0 0 20 20" width="20" height="20" fill="none" aria-hidden="true">'
    + `<path d="${path}" stroke="currentColor" stroke-width="1.5"`
    + ' stroke-linecap="square" stroke-linejoin="miter"/></svg>';
}

function ensureStyle(doc) {
  if (doc.getElementById(STYLE_ID)) return;
  const style = doc.createElement('style');
  style.id = STYLE_ID;
  style.textContent = CSS;
  doc.head.append(style);
}

function el(doc, tag, className) {
  const node = doc.createElement(tag);
  if (className) node.className = className;
  return node;
}

/**
 * Clip a string to `MAX_EXCERPT_CHARS` **code points**.
 *
 * See the constant. `String.prototype.slice` counts UTF-16 code units and will cut a
 * surrogate pair in half, which is the JS spelling of the byte-slicing that aborted the
 * desktop app on a real board.
 */
function clip(text) {
  const points = Array.from(text);
  if (points.length <= MAX_EXCERPT_CHARS) return text;
  return points.slice(0, MAX_EXCERPT_CHARS).join('') + '…';
}

/**
 * The answer from `find()`, validated.
 *
 * Exported because it is the whole of this file that is pure, and because every field here
 * has crossed a JSON boundary: a match whose `item` is not a number is a `focus_match` call
 * that cannot work, and a `where` that is not a string is a `textContent` write of the word
 * `undefined` onto a row.
 *
 * ⚠ **`item` is a scene id and scene ids are safe as JSON numbers**, which is worth stating
 * because `ItemId` is a `u64` and a `u64` in general is not. `Projection::add` assigns them
 * from a monotonic counter (`next_id += 1`), so a board would need 2^53 items before one
 * lost precision — and `focus_match(item: f64)` takes a `f64` on the Rust side, so the
 * round trip through a JS number is the contract rather than an accident of this file.
 *
 * Answers `null` for anything that is not a search result at all, which the caller reports
 * rather than showing as an empty board.
 */
export function parseAnswer(text) {
  if (typeof text !== 'string' || !text) return null;
  let raw;
  try {
    raw = JSON.parse(text);
  } catch {
    return null;
  }
  if (!raw || typeof raw !== 'object' || !Array.isArray(raw.matches)) return null;
  const matches = [];
  for (const hit of raw.matches) {
    if (!hit || typeof hit !== 'object') continue;
    const item = Number(hit.item);
    // Non-negative and integral: `focus_match` refuses a negative or a non-finite one, and a
    // fractional id is not an id at all. Dropping the row is better than drawing one whose
    // press cannot do anything.
    if (!Number.isFinite(item) || item < 0 || !Number.isInteger(item)) continue;
    matches.push({
      item,
      where: typeof hit.where === 'string' ? hit.where : '',
      excerpt: typeof hit.excerpt === 'string' ? clip(hit.excerpt) : '',
    });
  }
  const indexed = Number(raw.indexed);
  return {
    matches,
    indexed: Number.isFinite(indexed) && indexed >= 0 ? Math.floor(indexed) : 0,
    // ⚠ Strictly `true`, never truthy. A missing key means an older wasm that does not report
    // the cap at all, and reading that as "partial" would put a permanent warning under every
    // search on every board — an alarm that is always on is one nobody reads.
    partial: raw.partial === true,
  };
}

/**
 * The sentence shown when the index did not cover the whole board.
 *
 * Pure, and exported, so the words can be asserted without a DOM. They are words rather than
 * a colour on purpose — see the header.
 */
export function partialSentence(indexed) {
  const items = Number.isFinite(indexed) && indexed > 0 ? indexed.toLocaleString() : 'some';
  return `This board is larger than the search index: only ${items} items were searched, `
    + 'so there may be more matches than are listed here.';
}

/** `"3 of 12"` while stepping, `"12 matches"` before, `"No matches"` for nothing. */
export function countLabel(total, current) {
  if (total === 0) return 'No matches';
  if (current >= 0 && current < total) return `${current + 1} of ${total}`;
  return total === 1 ? '1 match' : `${total} matches`;
}

/** Take down whatever `mountFind` last put on the page. Safe to call twice. */
export function unmountFind() {
  if (mounted) mounted.unmount();
}

/**
 * Mount the find bar and wire it to the wasm module.
 *
 * `mod` is the module namespace object — the same one `index.html` holds as `velm`.
 *
 * Options:
 *   - `canvas`    the board's canvas, for insertion order. Required.
 *   - `document`  the document to build in. Defaults to the canvas's own, and exists so a
 *                 fixture can drive this file without a browser — `tools.js`'s arrangement,
 *                 and the reason its own A/B could be written at all.
 *   - `open`      whether to open the panel at mount. Defaults to `false`.
 *
 * Returns a handle — `{ container, toggle, panel, input, list, open, close, isOpen, search,
 * step, results, current, place, unmount }` — or `null` when there is no canvas. An object
 * rather than `mountChrome`'s single element because this mounts two surfaces and three
 * window listeners, and a caller that wants them gone should not have to know that.
 */
export function mountFind(mod, { canvas, document: docOption, open: openAtMount = false } = {}) {
  if (!mod || !canvas) return null;
  const doc = docOption || canvas.ownerDocument;
  if (!doc) return null;
  const win = doc.defaultView || (typeof window !== 'undefined' ? window : null);
  if (!win) return null;

  // ⚠ Idempotent. Without it a second call stacks a second panel **and** a second window
  // keydown listener, so one `⌘F` opens two panels and one Escape closes neither cleanly.
  unmountFind();
  ensureStyle(doc);

  // Gated separately, and both by name. See the header: a build with `find` and no
  // `focus_match` still lists what is on the board, which is worth having.
  const findName = bound(mod, 'find');
  const focusName = bound(mod, 'focus_match');
  const canFind = Boolean(findName);
  const canFocus = Boolean(focusName);

  const container = el(doc, 'div', CONTAINER_CLASS);

  // ------------------------------------------------------------------ the closed state

  const toggle = el(doc, 'button', 'velm-find-toggle');
  toggle.type = 'button';
  toggle.setAttribute('aria-label', 'Find on this board');
  toggle.setAttribute('aria-expanded', 'false');
  toggle.innerHTML = svg(ICON_FIND);
  if (canFind) {
    // The hint names the key as well as the verb, and it only ever appears under a mouse
    // pointer — which is the only kind of reader that has the key to press.
    toggle.title = 'Find on this board (⌘F)';
  } else {
    // ⚠ Drawn, disabled, and naming the export. Absent leaves the reader wondering whether
    // they mis-remembered; present and silent is worse than both.
    toggle.disabled = true;
    toggle.title = 'Find: this build has no find()';
  }

  // ------------------------------------------------------------------ the panel

  const panel = el(doc, 'div', 'velm-find-panel');
  // The `search` landmark, which is exactly what this is. Not a dialog: it is not modal, the
  // board underneath stays live, and announcing it as one would trap a screen reader in it.
  panel.setAttribute('role', 'search');
  panel.setAttribute('aria-label', 'Find on this board');
  panel.hidden = true;

  const row = el(doc, 'div', 'velm-find-row');

  const input = el(doc, 'input', 'velm-find-input');
  input.type = 'text';
  input.setAttribute('aria-label', 'Find on this board');
  input.placeholder = 'Find on this board';
  // A search keyboard on a tablet, and an Enter key that says what Enter does here.
  input.setAttribute('inputmode', 'search');
  input.setAttribute('enterkeyhint', 'search');
  // ⚠ All four off. Autocapitalise makes every query on iOS start with a capital, autocorrect
  // rewrites a part number into a word, and a spellcheck underline under a board's own
  // vocabulary says the board is misspelled.
  input.setAttribute('autocomplete', 'off');
  input.setAttribute('autocorrect', 'off');
  input.setAttribute('autocapitalize', 'off');
  input.setAttribute('spellcheck', 'false');

  const count = el(doc, 'span', 'velm-find-count');
  // Announced: it changes when the reader asks it to and each change is the answer to what
  // they just did. Unlike `chrome.js`'s zoom readout, which changes sixty times a second and
  // is deliberately `off`.
  count.setAttribute('aria-live', 'polite');
  count.textContent = '';

  const partialNote = el(doc, 'p', 'velm-find-partial');
  // Announced once, because the text is written only on a change — so a reader is told the
  // board was searched in part the first time it happens and not again on every keystroke.
  partialNote.setAttribute('aria-live', 'polite');
  partialNote.hidden = true;

  const note = el(doc, 'p', 'velm-find-note');
  note.setAttribute('aria-live', 'polite');
  note.hidden = true;

  const list = el(doc, 'div', 'velm-find-list');
  list.setAttribute('role', 'group');
  list.setAttribute('aria-label', 'Matches');
  list.hidden = true;

  /**
   * A control that hands the keyboard back to the field.
   *
   * ⚠ **Recorded before the click, restored after it**, and both halves matter. A click moves
   * focus to the button on `pointerdown`, so asking afterwards always answers "the button" —
   * and refocusing unconditionally would raise the on-screen keyboard on a tablet every time
   * somebody taps *Next*, covering the board they had just been taken to. So the field only
   * gets the keyboard back if it already had it, which is the keyboard user's case and never
   * the touch user's.
   *
   * The invariant this buys: **while the panel is open and the reader is typing, the field
   * keeps the keyboard.** ⇧Enter still steps backwards after five presses of Next, and no
   * bare letter is left sitting on a focused button where `tools.js`'s palette would arm a
   * tool with it.
   */
  let keyboardWasInField = false;
  const keepsKeyboard = (node) => {
    node.addEventListener('pointerdown', () => {
      keyboardWasInField = doc.activeElement === input;
    });
    return node;
  };
  const restoreKeyboard = () => {
    if (keyboardWasInField && typeof input.focus === 'function') input.focus();
  };

  const iconButton = (label, hint, path, run, available, missing) => {
    const node = el(doc, 'button', 'velm-find-btn');
    node.type = 'button';
    node.setAttribute('aria-label', label);
    node.innerHTML = svg(path);
    if (available) {
      node.title = hint;
      node.addEventListener('click', () => { run(); restoreKeyboard(); });
      keepsKeyboard(node);
    } else {
      node.disabled = true;
      node.title = `${label}: this build has no ${missing}()`;
    }
    return node;
  };

  // ------------------------------------------------------------------ state

  const state = {
    live: true,
    open: false,
    /// The last query actually sent to `find()`, so Enter can tell "step through what is
    /// already there" from "the field has moved on and has to be searched first".
    searched: null,
    matches: [],
    indexed: 0,
    partial: false,
    /// -1 until the reader asks to be taken somewhere. See `runSearch`: typing does **not**
    /// move the camera.
    current: -1,
    timer: 0,
    /// Whether the placement poll is already running, so opening a panel that is open does
    /// not start a second chain.
    polling: false,
    /// Written only on a change, the rule `chrome.js` states for its zoom readout — these
    /// are style writes inside a rAF poll, and one per frame is a layout per frame.
    placedLeft: '',
    placedTop: '',
    placedWidth: '',
    placedHeight: '',
    shownPartial: '',
    shownNote: '',
  };

  const rows = [];

  const setNote = (text) => {
    if (text === state.shownNote) return;
    state.shownNote = text;
    note.textContent = text;
    note.hidden = !text;
  };

  const setPartial = (text) => {
    if (text === state.shownPartial) return;
    state.shownPartial = text;
    partialNote.textContent = text;
    partialNote.hidden = !text;
  };

  // ------------------------------------------------------------------ placement

  /** A surface's rectangle, when it is on the page and drawn. */
  const surface = (selector) => {
    const node = doc.querySelector(selector);
    if (!node || node.hidden) return null;
    if (typeof node.getBoundingClientRect !== 'function') return null;
    const rect = node.getBoundingClientRect();
    return rect && rect.width > 0 && rect.height > 0 ? rect : null;
  };

  const boxOf = (node) => (typeof node.getBoundingClientRect === 'function'
    ? node.getBoundingClientRect()
    : { width: 0, height: 0, top: 0, left: 0, right: 0, bottom: 0 });

  /**
   * Put the panel where nothing else is.
   *
   * ⚠ **The top band is reserved unconditionally**, which is `tools.js`'s decision and its
   * reason: making it conditional closes a loop, because the horizontal clamps below depend
   * on the vertical placement. `chrome.js` owns one strip across the top-left and
   * `inspect.js`'s toggle keeps the top-right corner, so this sits under both of them and
   * costs a few pixels of height on a wide monitor to be certain of it.
   *
   * ⚠ **Every rectangle is read live and none is a constant.** The palette's width follows
   * the icon size and wraps into a second column on a short window; the properties panel is
   * `min(264px, 100vw - 24px)` and is not always up. A constant for either goes stale
   * silently, and what it produces is a field you can type into and cannot see.
   *
   * ⚠ **The panel narrows before it overlaps.** When the free span between the palette and
   * the properties panel is under `PANEL_WIDTH` the panel is squeezed into it rather than
   * pushed on top of one of them — down to `MIN_PANEL_WIDTH`, below which overlapping is the
   * better failure, because a field two characters wide is not a smaller find bar.
   */
  const place = () => {
    if (!state.live) return;
    const vw = win.innerWidth || 0;
    const vh = win.innerHeight || 0;
    if (!vw || !vh) return;

    let top = GAP;
    for (const rect of [surface('.velm-chrome'), surface('.velm-inspect-toggle')]) {
      if (rect) top = Math.max(top, rect.bottom + GAP);
    }

    const nextHeight = `${Math.round(Math.max(MIN_PANEL_HEIGHT, vh - top - GAP))}px`;
    if (nextHeight !== state.placedHeight) {
      state.placedHeight = nextHeight;
      panel.style.maxHeight = nextHeight;
    }

    // Measured after the height is written and before the width is, so `shares` is asked
    // about the height the panel will actually have. The width barely moves the height — the
    // list is capped by `max-height` either way — which is what makes one pass enough.
    const own = boxOf(container);
    const height = own.height || 0;
    const shares = (rect) => Boolean(rect) && top < rect.bottom && top + height > rect.top;

    let leftEdge = GAP;
    const palette = surface('.velm-tools');
    if (shares(palette)) leftEdge = palette.right + GAP;

    let rightEdge = vw - GAP;
    const properties = surface('.velm-inspect');
    if (shares(properties)) rightEdge = Math.min(rightEdge, properties.left - GAP);

    const span = Math.max(MIN_PANEL_WIDTH, rightEdge - leftEdge);
    const nextWidth = `${Math.round(span)}px`;
    if (nextWidth !== state.placedWidth) {
      state.placedWidth = nextWidth;
      panel.style.maxWidth = nextWidth;
    }

    const width = boxOf(container).width || 0;
    // Centred in the **free span** rather than in the window, so an open properties panel
    // pushes the find bar left instead of leaving it sitting against the panel's edge.
    const centred = leftEdge + (rightEdge - leftEdge - width) / 2;
    // `Math.max(leftEdge, …)` and not the right limit alone: on a narrow window the two
    // limits cross, and the left one wins — `tools.js`'s rule, for its reason. A surface
    // pushed off the right edge is unreachable; one that overlaps is still usable.
    const x = Math.min(Math.max(centred, leftEdge), Math.max(leftEdge, rightEdge - width));

    const nextLeft = `${Math.round(x)}px`;
    const nextTop = `${Math.round(top)}px`;
    if (nextLeft !== state.placedLeft) {
      state.placedLeft = nextLeft;
      container.style.left = nextLeft;
    }
    if (nextTop !== state.placedTop) {
      state.placedTop = nextTop;
      container.style.top = nextTop;
    }
  };

  // ------------------------------------------------------------------ searching

  /**
   * Run the query, now.
   *
   * ⚠ **Guarded whole.** `panic = "abort"` poisons a wasm module, so once one export throws
   * every export throws — and the case where the page is most broken is the case where this
   * is least able to say so. `chrome.js` records the same trap on its sync indicator.
   */
  const runSearch = (query) => {
    if (!canFind) return;
    win.clearTimeout(state.timer);
    state.timer = 0;
    const trimmed = query.trim();
    if (!trimmed) {
      // Not sent. `find("")` answers an empty result on the Rust side before it looks at
      // anything, so this is not a correctness guard — it is one fewer crossing of the wasm
      // boundary on the most common keystroke there is, the one that clears the field.
      state.searched = '';
      state.matches = [];
      state.indexed = 0;
      state.partial = false;
      state.current = -1;
      setPartial('');
      setNote('');
      draw();
      return;
    }
    let answer;
    try {
      answer = parseAnswer(mod[findName](trimmed));
    } catch (error) {
      state.searched = trimmed;
      state.matches = [];
      state.current = -1;
      setPartial('');
      setNote(`Search stopped answering: ${String(error)}`);
      draw();
      return;
    }
    if (!answer) {
      state.searched = trimmed;
      state.matches = [];
      state.current = -1;
      setPartial('');
      setNote('Search did not answer with a result this page understands.');
      draw();
      return;
    }
    state.searched = trimmed;
    state.matches = answer.matches;
    state.indexed = answer.indexed;
    state.partial = answer.partial;
    // ⚠ **Typing does not move the camera**, and that is a decision rather than an omission.
    // A browser's find bar jumps to the first hit per character; every jump here is a
    // `fit_to_rect` and a repaint of a board, and it would take the reader off the thing they
    // were looking at while they were still typing what to look for. Enter goes to the first.
    state.current = -1;
    setPartial(answer.partial ? partialSentence(answer.indexed) : '');
    setNote('');
    draw();
  };

  /** Search after `SEARCH_DEBOUNCE_MS` of quiet. See the header for the number. */
  const searchSoon = () => {
    win.clearTimeout(state.timer);
    const query = input.value;
    // An emptied field answers immediately: there is nothing to send and nothing to wait
    // for, and a list that lingers for a fifth of a second after the field is cleared reads
    // as the clear not having worked.
    if (!query.trim()) {
      runSearch(query);
      return;
    }
    state.timer = win.setTimeout(() => {
      state.timer = 0;
      runSearch(input.value);
    }, SEARCH_DEBOUNCE_MS);
  };

  /**
   * Bring the field's contents up to date before acting on the results.
   *
   * ⚠ Enter must not step through the answer to a query the reader has already typed past.
   * Typing `coolant` and pressing Enter before the debounce fires would otherwise step
   * through the matches for `coolan` — or, on the first Enter, through nothing at all.
   */
  const flush = () => {
    const query = input.value.trim();
    if (state.timer || state.searched !== query) runSearch(input.value);
  };

  // ------------------------------------------------------------------ stepping

  /** Take the camera to match `index`, and mark the row. */
  const go = (index) => {
    if (!canFocus || !state.matches.length) return;
    const total = state.matches.length;
    // Wrapped, both ways: `((i % n) + n) % n` rather than `i % n`, because JS's remainder
    // keeps the sign and ⇧Enter on the first match would otherwise ask for -1.
    const next = ((index % total) + total) % total;
    const hit = state.matches[next];
    state.current = next;
    let landed = false;
    try {
      landed = mod[focusName](hit.item) !== false;
    } catch (error) {
      setNote(`Could not show that match: ${String(error)}`);
      mark();
      return;
    }
    if (!landed) {
      // ⚠ `focus_match` answers `false` when the projection no longer holds that id — the
      // board has moved under the results. A row that silently does nothing is the inert
      // control this house style forbids, so the query is re-run against the board as it is
      // now and the reader is told why the list changed.
      //
      // ⚠ **The sentence is written after the re-run, not before it.** `runSearch` clears the
      // transient note on its way out — which is right, a new answer is a new situation — so
      // setting it first is setting it and then deleting it in the same call.
      const query = state.searched;
      if (query) runSearch(query);
      setNote('That object is no longer on the board. These results are from a moment ago, '
        + 'so they have been searched again.');
      return;
    }
    setNote('');
    mark();
  };

  const step = (delta) => {
    if (!canFocus) return;
    flush();
    if (!state.matches.length) return;
    // From nothing, forwards lands on the first and backwards on the last, which is what
    // every find bar does and what "previous" means when you have not been anywhere yet.
    if (state.current < 0) go(delta >= 0 ? 0 : state.matches.length - 1);
    else go(state.current + (delta >= 0 ? 1 : -1));
  };

  // ------------------------------------------------------------------ drawing

  /** The current row's tint, its `aria-current`, and the counter. */
  const mark = () => {
    rows.forEach((node, index) => {
      node.setAttribute('aria-current', String(index === state.current));
    });
    count.textContent = countLabel(state.matches.length, state.current);
    const row = rows[state.current];
    // `nearest`, so a row already on screen does not scroll the list under the reader.
    if (row && typeof row.scrollIntoView === 'function') row.scrollIntoView({ block: 'nearest' });
  };

  function draw() {
    // ⚠ Emptied by `textContent`, which detaches the old rows and their listeners with them.
    // Never `innerHTML = ''` — same effect, and it puts a string parse on a path that is
    // about to have document text written into it.
    list.textContent = '';
    rows.length = 0;

    if (!state.matches.length) {
      list.hidden = !state.searched;
      if (state.searched) {
        const empty = el(doc, 'div', 'velm-find-empty');
        empty.textContent = 'Nothing on this board matches that.';
        list.append(empty);
      }
      count.textContent = state.searched ? countLabel(0, -1) : '';
      place();
      return;
    }

    for (let index = 0; index < state.matches.length; index += 1) {
      const hit = state.matches[index];
      const node = el(doc, 'button', 'velm-find-item');
      node.type = 'button';

      const where = el(doc, 'span', 'velm-find-where');
      // ⚠ **`textContent`, and this is the line it matters on.** Both strings are document
      // text: an item's own words and the name of the frame it sits on, written by whoever
      // made the board and carried here through a paste, an import or a sync. `innerHTML`
      // here is a script tag in a sticky note.
      where.textContent = hit.where;

      const excerpt = el(doc, 'span', 'velm-find-excerpt');
      excerpt.textContent = hit.excerpt;

      node.append(where, excerpt);
      // The row's own accessible name, so a screen reader is not read two spans with no
      // relationship between them.
      node.setAttribute('aria-label', hit.excerpt ? `${hit.excerpt}, ${hit.where}` : hit.where);
      node.setAttribute('aria-current', 'false');

      if (canFocus) {
        node.addEventListener('click', () => { go(index); restoreKeyboard(); });
        keepsKeyboard(node);
      } else {
        // Listed, so the reader still learns what is on the board, and disabled with the
        // export named — the row cannot take them there and says which function is missing.
        node.disabled = true;
        node.title = 'Showing a match needs focus_match(), which this build has not got';
      }
      rows.push(node);
      list.append(node);
    }

    list.hidden = false;
    mark();
    place();
  }

  // ------------------------------------------------------------------ opening and closing

  const openPanel = ({ focus = true } = {}) => {
    if (!canFind || !state.live) return;
    state.open = true;
    toggle.hidden = true;
    toggle.setAttribute('aria-expanded', 'true');
    panel.hidden = false;
    place();
    if (focus && typeof input.focus === 'function') {
      input.focus();
      // Selected, not appended to: reopening the bar to search for something else is the
      // common case, and a reader who wants to add a word can press End. This is what every
      // find bar on the platform does.
      if (typeof input.select === 'function') input.select();
    }
    startPoll();
  };

  const closePanel = () => {
    if (!state.live) return;
    state.open = false;
    win.clearTimeout(state.timer);
    state.timer = 0;
    panel.hidden = true;
    toggle.hidden = false;
    toggle.setAttribute('aria-expanded', 'false');
    // ⚠ The results are kept, deliberately. Reopening with `⌘F` shows the same list, which
    // is what makes the bar something to close while you look at the board rather than
    // something to hold open — and there is no cost to keeping them, because the index is
    // rebuilt from the board's generation and not from this array.
    place();
  };

  // ------------------------------------------------------------------ keys

  const onFieldKeyDown = (event) => {
    if (event.key === 'Enter') {
      // Bound only when there is somewhere to be taken. A key that silently does nothing is
      // the same broken promise as a button that does — `chrome.js` states the rule and
      // leaves an unbound key to whatever else wants it.
      if (!canFocus) return;
      event.preventDefault();
      step(event.shiftKey ? -1 : 1);
      return;
    }
    // ⚠ **There is deliberately no Escape branch here.** The window handler below takes
    // Escape in the *capture* phase and calls `stopPropagation`, so the key never reaches
    // this field at all — a second handler for it would be dead code that reads like a
    // guard, which is the shape this repository keeps finding at review time.
  };

  /**
   * ⚠ **Capture, and `stopPropagation` only while the panel is open.**
   *
   * `chrome.js` binds Escape to *leave the board* and registered first, so an Escape meant
   * to close this panel would navigate the reader off the page they are reading. Taking the
   * key unconditionally is the other half of the same mistake: Escape with nothing open is
   * still the way out.
   *
   * ⚠ **`defaultPrevented` is what makes this a ladder rather than a race.** `tools.js` also
   * listens in capture and calls `preventDefault` when it closes a menu; a `stopPropagation`
   * does not stop other listeners on the *same* node, so without this check one Escape would
   * close its menu and this panel together. Whichever surface claimed the key first keeps it.
   */
  const onKeyDown = (event) => {
    if (event.defaultPrevented) return;
    if (event.key === 'Escape') {
      if (!state.open) return;
      event.preventDefault();
      event.stopPropagation();
      closePanel();
      return;
    }
    // ⚠ **`preventDefault`, or the browser's own find bar opens on top of this one** — and
    // then the reader is typing into a field that searches the *rendered page*, which for a
    // board drawn on a canvas is a page containing no text at all.
    //
    // Only when this build can search. A build without `find()` must leave `⌘F` to the
    // browser: swallowing the key and offering nothing takes away a working feature.
    if (!canFind) return;
    if (event.altKey) return;
    // Exactly one of the two, so `⌃⌘F` — full screen on macOS — is not claimed here.
    const chord = Boolean(event.metaKey) !== Boolean(event.ctrlKey);
    if (!chord) return;
    // `code` as well as `key`: on a non-Latin layout `event.key` for the F key is not `f`,
    // and `⌘F` is muscle memory that does not change with the layout.
    const key = String(event.key || '').toLowerCase();
    if (key !== 'f' && event.code !== 'KeyF') return;
    event.preventDefault();
    event.stopPropagation();
    openPanel();
  };

  // ------------------------------------------------------------------ wiring

  input.addEventListener('input', searchSoon);
  input.addEventListener('keydown', onFieldKeyDown);

  const prev = iconButton('Previous match', 'Previous match (⇧Enter)', ICON_PREV,
    () => step(-1), canFocus, 'focus_match');
  const next = iconButton('Next match', 'Next match (Enter)', ICON_NEXT,
    () => step(1), canFocus, 'focus_match');
  const close = iconButton('Close find', 'Close find (Esc)', ICON_CLOSE,
    closePanel, true, '');

  row.append(input, count, prev, next, close);
  panel.append(row, partialNote, note, list);
  container.append(toggle, panel);

  toggle.addEventListener('click', () => openPanel());

  // A right-click on a Velm control raising *Reload* and *View Source* is the tell that the
  // control is not really part of the application — `tools.js`'s line, for its reason. The
  // field is exempt: its menu is Cut, Copy, Paste, which is the one place that menu is right.
  for (const node of [toggle, row, list]) {
    node.addEventListener('contextmenu', (event) => {
      if (event.target !== input) event.preventDefault();
    });
  }

  win.addEventListener('keydown', onKeyDown, true);
  const onResize = () => place();
  win.addEventListener('resize', onResize);
  win.addEventListener('orientationchange', onResize);

  // After the canvas, like `chrome.js` and `tools.js`, and with no `z-index`: a positioned
  // element paints above the static canvas on DOM order alone, and the page's own fixed
  // panels — the status line, the board list — still paint above these.
  canvas.insertAdjacentElement('afterend', container);

  /**
   * ⚠ **A rAF poll, and only while the panel is open.**
   *
   * `schedule_frame` in `lib.rs` chains `requestAnimationFrame` unconditionally for the life
   * of the page, so this adds a callback to a frame the browser was already going to run —
   * `chrome.js`'s argument for its zoom readout. What it must not do is run while the panel
   * is closed: the reads are four `getBoundingClientRect()`s, which force a layout, and a
   * closed find bar has nothing to follow. It exists at all because the properties panel can
   * open and close underneath this one, and neither of them tells the other.
   */
  const startPoll = () => {
    if (typeof win.requestAnimationFrame !== 'function') return;
    if (state.polling) return;
    state.polling = true;
    const poll = () => {
      if (!state.live || !state.open) {
        state.polling = false;
        return;
      }
      place();
      win.requestAnimationFrame(poll);
    };
    win.requestAnimationFrame(poll);
  };

  const session = {
    container,
    toggle,
    panel,
    input,
    list,
    open: (options) => openPanel(options),
    close: closePanel,
    isOpen: () => state.open,
    /** Search now, skipping the debounce. For a fixture, and for Enter. */
    search: (query) => {
      if (typeof query === 'string') input.value = query;
      runSearch(input.value);
    },
    /** Step to the next (`1`) or previous (`-1`) match. */
    step,
    /** The matches as parsed, for a fixture. A copy: the caller must not edit the state. */
    results: () => state.matches.slice(),
    /** The index of the match the camera was last taken to, or -1. */
    current: () => state.current,
    place,
    unmount() {
      if (!state.live) return;
      state.live = false;
      state.open = false;
      // ⚠ The flag first, then the timer. A debounced search may already be scheduled when
      // the timer is cleared, and `chrome.js` records the same trap: a second mount left the
      // first one's timer writing into an element that had left the document.
      win.clearTimeout(state.timer);
      state.timer = 0;
      win.removeEventListener('keydown', onKeyDown, true);
      win.removeEventListener('resize', onResize);
      win.removeEventListener('orientationchange', onResize);
      container.remove();
      if (mounted === session) mounted = null;
    },
  };
  mounted = session;

  place();
  if (openAtMount) openPanel({ focus: false });
  return session;
}
