// Chrome for the browser viewer: a way back to the board list, and the view controls.
//
// The page this mounts into is a canvas, a status line and nothing else, so until it existed
// **there was no way back to the board list once a board was open** — you edited the URL by
// hand, which on the tablet this port exists for means summoning a keyboard to delete a query
// parameter. That is the gap. The zoom controls come with it because the same person on the
// same tablet has neither a wheel nor a `0` key, and `input.rs` offers zoom through those two
// and a pinch alone.
//
// # One small bar, and it is opaque
//
// `docs/05-design-language.md` §3a puts a floating zoom cluster in the *yes* column for glass.
// This is deliberately not that, for the reason the same section gives when it moves menus out
// of that column: **legibility wins over the material, every time.** A `backdrop-filter` here
// is a second full-screen composite every frame, in a tab whose frame budget is the thing this
// port is already worst at, bought for a translucent 50px strip in the corner of the board a
// reader is least likely to be looking at. §3a's own performance clause — *a blur that costs
// frame time is not worth having* — decides it before taste does.
//
// # ⚠ Nothing here is a gesture the viewer cannot answer
//
// Every control is drawn only if the wasm module actually exports the function behind it.
// `zoom_by` and `fit_board` are newer than this file, and a build without them draws no zoom
// cluster rather than three buttons that throw. The same test gates the **keys**: a `+` that
// silently does nothing is the same broken promise as a button that does, and CLAUDE.md names
// describing a gesture the user cannot perform as the worst failure in its own list.
//
// # ⚠ Touch: the bar must not eat the board
//
// The container is `pointer-events: none` and only the buttons take it back. A review found
// the status line eating the bottom-left corner of the board for want of exactly that line,
// and the mechanism is worse than a dead patch: a touch pointer gets implicit capture on
// whatever it pressed, so one finger of a pinch landing on chrome sends that finger's whole
// stream to the chrome, `input.rs` sees a single contact, and the board **pans when the user
// meant to zoom**. So the label, the dividers and the percentage stay transparent to a
// pointer, which also costs the board name its hover tooltip — a long name truncates and
// cannot be read in full, and that is the right side of the trade.
//
// The bar sits **top-left**: where `docs/04-ui-reference.md` §4 puts the desktop app's board
// name, and nowhere a thumb rests on a tablet held in either orientation. The status line
// already owns the bottom-left, and the bottom edge as a whole is the one place this could
// not go.
//
// # Colours are the page's, not the theme's
//
// `#22282C`, `#656D73`, `#E2E7EA` are `index.html`'s own tokens rather than `theme.rs`'s
// `ink` / `ink-muted` / `frost`, which sit two or three levels away from each of them. This
// design draws its structure with hairlines (§2: *a hairline where other designs use a
// shadow*), so two nearly-identical hairlines on one screen read as a mistake rather than as
// a system. There is no `prefers-color-scheme` block for the same reason: the page it lives
// in has none, and a bar that flips to dark over a light board is worse than one that does
// not flip at all.

const STYLE_ID = 'velm-chrome-style';
const BAR_CLASS = 'velm-chrome';

/// One press of the zoom buttons, and its exact inverse for the other direction.
///
/// In and out are `k` and `1/k` rather than two tuned numbers, so pressing in and then out
/// returns to the zoom you started at — the same property `input.rs` argues for at length
/// when it makes the wheel exponential in its delta rather than linear.
const ZOOM_STEP = 1.25;

/// The bar currently on the page, so a second `mountChrome` can take the first one down.
let mounted = null;

const CSS = `
.velm-chrome {
  position: fixed;
  /* max(), not a bare number: the inset is 0 on a plain monitor and the notch's width on an
     iPad held in landscape, where 12px alone puts the bar under the rounded corner. */
  top: max(12px, env(safe-area-inset-top));
  left: max(12px, env(safe-area-inset-left));
  display: flex;
  align-items: center;
  /* 6, because feedback 38 moved the desktop selection bar from 2 to 6 on the user's own
     "keep the dividers, use more whitespace". */
  gap: 6px;
  padding: 4px;
  max-width: calc(100vw - 24px);
  /* 10, not §2's 4-6. Feedback 38 raised the desktop bar's radius from 6 to 10 against a
     Miro screenshot, and that is later than the doc and about exactly this surface: a small
     bar floating over a board. */
  border-radius: 10px;
  background: #fff;
  border: 1px solid #E2E7EA;
  /* §3a: tight, plus one very soft ambient pass. Glass floats; it does not hover in mid-air.
     The second pair is feedback 38's (0,2) blur 8. */
  box-shadow: 0 1px 2px rgba(26, 29, 31, .06), 0 2px 8px rgba(26, 29, 31, .08);
  font: 13px/1.4 -apple-system, BlinkMacSystemFont, "Segoe UI", Inter, sans-serif;
  color: #22282C;
  /* ⚠ The whole reason this bar can sit over the board at all. Taken back by the buttons
     below and by nothing else. */
  pointer-events: none;
  -webkit-user-select: none;
  user-select: none;
}

.velm-chrome-btn {
  pointer-events: auto;
  appearance: none;
  -webkit-appearance: none;
  display: inline-flex;
  align-items: center;
  justify-content: center;
  gap: 6px;
  /* 44px, Apple's own minimum, and the reason this bar is taller than its desktop sibling:
     the context bar's controls are 32pt (feedback 28) because they are aimed with a mouse.
     The design doc's "no oversized whitespace" is about layout, not about hit targets. */
  min-width: 44px;
  height: 44px;
  /* ⚠ The controls never give. On a narrow phone the bar is wider than the window, and
     flexbox has to take the difference out of something: min-width 0 on the name makes it
     the only candidate, so a long title ellipsises and every button keeps its 44px. Without
     this the squeeze is shared and all three shrink below the touch minimum at once. */
  flex: none;
  padding: 0;
  margin: 0;
  border: 0;
  border-radius: 6px;
  background: transparent;
  color: inherit;
  font: inherit;
  text-decoration: none;
  cursor: pointer;
  /* §3: motion is functional and fast, 120-160ms, ease-out. */
  transition: background-color 120ms ease-out;
  /* iOS otherwise paints its own grey flash over the top of every state below. */
  -webkit-tap-highlight-color: transparent;
  /* Drops the 300ms wait a browser holds in case the tap was the first half of a
     double-tap-to-zoom, which on a page that already handles its own zoom is pure latency. */
  touch-action: manipulation;
}

/* ⚠ Guarded, because :hover on a touchscreen latches: a tapped button keeps the hover wash
   until something else is tapped, so the bar shows a control as pointed-at that nothing is
   pointing at. */
@media (hover: hover) {
  .velm-chrome-btn:hover { background: #F2F5F6; }
}
.velm-chrome-btn:active { background: #E9EEF0; }
/* §6: visible focus rings in monitor-cyan. Inset, so the ring cannot fall outside the bar's
   own 4px padding and be clipped by its radius. */
.velm-chrome-btn:focus-visible { outline: 2px solid #6FD6E6; outline-offset: -2px; }
.velm-chrome-btn[disabled] { color: #8B959B; cursor: default; }
.velm-chrome-btn[disabled]:hover { background: transparent; }

.velm-chrome-btn svg { display: block; width: 20px; height: 20px; flex: none; }

.velm-chrome-back { padding: 0 12px; }
.velm-chrome-back-label { font-weight: 500; }
/* On a phone the board's own name is worth more than the word beside an arrow that already
   means "back" on every screen the reader has ever held. */
@media (max-width: 420px) {
  .velm-chrome-back { padding: 0; }
  .velm-chrome-back-label { display: none; }
}

.velm-chrome-name {
  min-width: 0;
  max-width: 34vw;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  font-weight: 600;
  padding: 0 2px;
}

.velm-chrome-rule {
  flex: none;
  width: 1px;
  height: 20px;
  background: #E2E7EA;
}

/* The connection, as a dot and a word. Muted by default: a board that is keeping up is the
   normal state and should not be the loudest thing on the bar. Only a *problem* is coloured,
   and it is coloured amber rather than with the accent — the accent means selection or the
   active tool everywhere else in Velm, and a viewer has neither. */
.velm-chrome-sync {
  flex: none;
  display: inline-flex;
  align-items: center;
  gap: 5px;
  padding-right: 4px;
  font: 12px/1.4 -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, sans-serif;
  color: #656D73;
  white-space: nowrap;
}
.velm-chrome-sync::before {
  content: '';
  width: 6px;
  height: 6px;
  border-radius: 50%;
  background: currentColor;
  flex: none;
}
.velm-chrome-sync[data-state='trouble'] { color: #A8620E; }
.velm-chrome-sync[data-state='offline'] { color: #A8320E; }
/* Narrow windows: the word goes and the dot stays, because the dot is the part that says
   "something is wrong" at a glance and the bar must not push the board off the screen. */
@media (max-width: 520px) {
  .velm-chrome-sync .velm-chrome-sync-word { display: none; }
  .velm-chrome-sync { gap: 0; }
}

.velm-chrome-zoom {
  flex: none;
  min-width: 5.5ch;
  padding-right: 6px;
  text-align: right;
  /* §3: monospace for anything numeric, tabular figures so the digits do not jitter while
     the board is being dragged. Same stack the status line uses. */
  font: 12px/1.4 ui-monospace, SFMono-Regular, Menlo, monospace;
  font-variant-numeric: tabular-nums;
  color: #656D73;
}

@media (prefers-reduced-motion: reduce) {
  .velm-chrome-btn { transition: none; }
}
`;

// Icons: line drawings on a 20 grid, 1.5px stroke, square caps — §3's "drawn as a set, not
// collected", and the same caps and mitres `assets/logo/mark.svg` uses.
//
// None of them is accented, and that is a decision rather than an omission. In Velm the
// accent means *selection* or *the active tool*, and a read-only viewer has neither; colour
// applied for emphasis alone is precisely the "looks current" failure §2 rules out.
const ICON_BACK = 'M16 10H5m4.5-4.5L5 10l4.5 4.5';
const ICON_MINUS = 'M5 10h10';
const ICON_PLUS = 'M10 5v10M5 10h10';
/// Four corner brackets around a frame that is never drawn — the Velm mark, scaled from its
/// 48 grid to this one. Fit-to-board *is* the mark's own subject: a bounded view onto
/// something with no edges.
const ICON_FIT = 'M3.5 8.5V3.5H8.5M11.5 3.5H16.5V8.5M16.5 11.5V16.5H11.5M8.5 16.5H3.5V11.5';

function svg(path) {
  return '<svg viewBox="0 0 20 20" width="20" height="20" fill="none" aria-hidden="true">'
    + `<path d="${path}" stroke="currentColor" stroke-width="1.5"`
    + ' stroke-linecap="square" stroke-linejoin="miter"/></svg>';
}

function ensureStyle() {
  if (document.getElementById(STYLE_ID)) return;
  const style = document.createElement('style');
  style.id = STYLE_ID;
  style.textContent = CSS;
  document.head.append(style);
}

function divider() {
  const rule = document.createElement('span');
  rule.className = 'velm-chrome-rule';
  rule.setAttribute('aria-hidden', 'true');
  return rule;
}

function button(label, hint, path, run) {
  const el = document.createElement('button');
  el.type = 'button';
  el.className = 'velm-chrome-btn';
  el.setAttribute('aria-label', label);
  // The hint names the key as well as the verb, and it only ever appears under a mouse
  // pointer — which is the only kind of reader that has the key to press.
  el.title = hint;
  el.innerHTML = svg(path);
  el.addEventListener('click', run);
  return el;
}

/**
 * The camera's zoom as a percentage.
 *
 * One decimal below ten and none above it. A board opens fitted at about 4%, where 4% and 5%
 * are a quarter apart and a whole number is a coarse readout; at 100% the decimal is noise.
 * The width is held steady by `tabular-nums` and a `min-width` rather than by padding the
 * string, so the number stays right-aligned against the bar's edge as it crosses 10 and 100.
 */
function percent(zoom) {
  const value = zoom * 100;
  return (value < 9.95 ? value.toFixed(1) : String(Math.round(value))) + '%';
}

/**
 * Mount the viewer's chrome and wire it to the wasm module.
 *
 * `mod` is the module namespace object — the same one `index.html` holds as `velm`. Which
 * controls appear is decided by which functions it actually exports, so this file can ship
 * before `zoom_by` and `fit_board` do.
 *
 * Returns the bar element, or `null` when there is no canvas to mount over.
 */
export function mountChrome(mod, { canvas, boardId, boardTitle, token } = {}) {
  if (!mod || !canvas) return null;

  // ⚠ Idempotent. A second call would otherwise stack a second bar **and** a second `keydown`
  // listener on the window, and two listeners means one `+` zooms twice — which reads as the
  // zoom step being wrong rather than as the bar being mounted twice.
  if (mounted) {
    mounted.live = false;
    mounted.bar.remove();
    window.removeEventListener('keydown', mounted.onKeyDown);
    // The rAF poll stops on its own through `session.live`; a `setTimeout` chain does not —
    // it is already scheduled. Without this a second `mountChrome` leaves the first one's
    // timer writing into a span that has been removed from the document.
    if (mounted.syncTimer) window.clearTimeout(mounted.syncTimer);
    mounted = null;
  }

  ensureStyle();

  const canZoom = typeof mod.zoom_by === 'function';
  const canFit = typeof mod.fit_board === 'function';
  const canRead = typeof mod.camera_report === 'function';
  // The connection, when the build has one. A static `./board.bin` page has no server, so
  // `sync_status` answers "off" and the dot is not drawn at all — an indicator that is
  // permanently grey says less than no indicator.
  const canSync = typeof mod.sync_status === 'function';
  // ⚠ Back is gated on there being a board list to go back *to*. On the static `./board.bin`
  // route there is no server: `showLibrary()` fails, `index.html` falls through to the same
  // board, and the button is one that reloads the page you are already on. A board id is what
  // says `/api/v1/boards` answered.
  const canGoBack = Boolean(boardId);

  const bar = document.createElement('div');
  bar.className = BAR_CLASS;
  bar.setAttribute('role', 'toolbar');
  bar.setAttribute('aria-label', 'Board view');

  // The board list, carrying the token and nothing else. `board`, `selftest`, `zoom`, `cx`
  // and `cy` all name *this* board or a fixture to run on it — carrying `selftest=touch` back
  // would start a gesture fixture on a page that has hidden its canvas.
  // ⚠ `./boards.html`, not this page's own path. Back used to reload index.html with the
  // board dropped, which fell through to its inline placeholder list — right when there was
  // no picker and wrong now that there is one. `canGoBack` already suppresses the button on
  // the static `./board.bin` route, so this adds no dead link.
  const listUrl = new URL('./boards.html', location.href);
  if (token) listUrl.searchParams.set('token', token);

  let back = null;
  if (canGoBack) {
    // An `<a href>`, not a `<button>`, because this navigates: the browser's own Back then
    // returns to the board, ⌘-click and middle-click open the list in a new tab, and the
    // destination is visible on hover. None of that is reachable from a click handler.
    back = document.createElement('a');
    back.className = 'velm-chrome-btn velm-chrome-back';
    back.href = listUrl.href;
    back.title = 'All boards (Esc)';
    back.setAttribute('aria-label', 'All boards');
    back.innerHTML = svg(ICON_BACK) + '<span class="velm-chrome-back-label">Boards</span>';
  }

  // The title when the caller has one, the id when it does not, and nothing at all when
  // neither exists — a chip reading "Board" over a board names nothing.
  let name = null;
  if (boardTitle || boardId) {
    name = document.createElement('span');
    name.className = 'velm-chrome-name';
    // ⚠ `textContent`. This is the one string on the bar that somebody else wrote, and it
    // arrives over the wire from `/api/v1/boards`.
    name.textContent = boardTitle || boardId;
  }

  // Whether there is a camera to move yet. `camera_report()` answers `"none"` until `boot`
  // has finished its GPU handshake and its fetch, and a zoom pressed before then reaches a
  // viewer that does not exist. Only gated when the camera can be read at all: with no reader
  // there is no moment to wait for, and controls disabled forever are worse than controls
  // enabled a little early.
  let ready = !canRead;

  // ⚠ **The wait belongs to the actions that touch the camera, not to the keyboard
  // handler.** One derivation, shared by both doors: the buttons also carry `disabled`, but
  // that is the *appearance* of the rule and this is the rule, because a keystroke has no
  // disabled attribute to respect. Escape is deliberately outside it — see below.
  const needsCamera = (run) => () => { if (ready) run(); };
  const zoomIn = needsCamera(() => mod.zoom_by(ZOOM_STEP));
  const zoomOut = needsCamera(() => mod.zoom_by(1 / ZOOM_STEP));
  const fitBoard = needsCamera(() => mod.fit_board());

  const controls = [];
  if (canZoom) controls.push(button('Zoom out', 'Zoom out (-)', ICON_MINUS, zoomOut));
  if (canFit) controls.push(button('Fit board to the screen', 'Fit board (0)', ICON_FIT, fitBoard));
  if (canZoom) controls.push(button('Zoom in', 'Zoom in (+)', ICON_PLUS, zoomIn));
  for (const control of controls) control.disabled = !ready;

  // ⚠ **The one thing that tells you a board has stopped keeping up.**
  //
  // Without it, a stopped server, an expired token or an update that will not merge look
  // exactly like a board nobody is editing: the poll backs off to a minute and keeps trying,
  // the console carries a warning, and on an iPad there is no console. The boot line in
  // `#velm-status` still reads "1130 items · 4531 pixels painted", which reads as healthy.
  //
  // A dot and a word rather than a dot alone: "live" and "offline" are two states a colour
  // has to carry on its own otherwise, and this palette has one accent.
  let connection = null;
  if (canSync) {
    connection = document.createElement('span');
    connection.className = 'velm-chrome-sync';
    // Announced, unlike the zoom readout — this changes a handful of times an hour and each
    // change is something the reader would want to know. The zoom changes sixty times a
    // second, which is why that one is `off`.
    connection.setAttribute('aria-live', 'polite');
    connection.textContent = '';
  }

  let readout = null;
  if (canRead) {
    readout = document.createElement('span');
    readout.className = 'velm-chrome-zoom';
    // ⚠ Not a live region. The camera changes on every frame of a pan, and a screen reader
    // asked to announce that would talk continuously for the length of the gesture.
    readout.setAttribute('aria-live', 'off');
    readout.textContent = '—';
  }

  // Groups, joined by a divider only where there is something on both sides of it. Built this
  // way rather than by appending rules inline because every group above is conditional, and a
  // divider against the bar's own edge is the tell that one of them was not drawn.
  const groups = [];
  if (back) groups.push([back]);
  if (name) groups.push([name]);
  const view = controls.concat(readout ? [readout] : []);
  if (view.length) groups.push(view);
  if (connection) groups.push([connection]);
  if (!groups.length) return null;
  groups.forEach((group, index) => {
    if (index > 0) bar.append(divider());
    bar.append(...group);
  });

  const keys = new Map();
  if (canZoom) {
    // `+` needs Shift on most layouts, so `=` is the key actually under the finger; both are
    // bound, as every application offering this pair does. A numeric keypad reports its own
    // `+` and `-` as these same two characters through `event.key`, so it needs no arm.
    keys.set('+', zoomIn);
    keys.set('=', zoomIn);
    keys.set('-', zoomOut);
    keys.set('_', zoomOut);
  }
  if (canFit) keys.set('0', fitBoard);
  // ⚠ Escape is **not** behind `ready`, and the Back link is not disabled either. Leaving
  // is navigation: it needs no camera, no board and no GPU. Boot is a device handshake plus
  // a fetch over whatever wifi the tablet is on, so someone pressing Escape while the status
  // line still reads "Fetching the board…" is exactly the person who most wants out. Gating
  // the handler instead of the actions made Back work from the button and do nothing from
  // the key — two doors to one verb disagreeing, which is feedback 40's shape.
  if (canGoBack) keys.set('Escape', () => location.assign(back.href));

  const onKeyDown = (event) => {
    // ⚠ ⌘/⌃ chords belong to the browser: ⌘+ and ⌘- are its own page zoom and ⌘0 resets it,
    // and swallowing those would take the one zoom that works when this bar does not. Shift
    // is deliberately not in the list — `+` is a shifted key on a US layout.
    if (event.metaKey || event.ctrlKey || event.altKey) return;
    // Nothing on this page takes typed text today. This is the line that stops the first
    // field that does — a search box, a rename — from zooming the board instead of typing.
    const target = event.target;
    if (target instanceof HTMLElement
      && (target.isContentEditable || /^(INPUT|TEXTAREA|SELECT)$/.test(target.tagName))) {
      return;
    }
    const run = keys.get(event.key);
    // Unbound keys are left alone rather than swallowed, which is the same rule the buttons
    // follow: a build whose wasm has no `fit_board` must leave `0` to whatever else wants it.
    if (!run) return;
    event.preventDefault();
    run();
  };
  window.addEventListener('keydown', onKeyDown);

  const session = { live: true, bar, onKeyDown };
  mounted = session;

  if (canRead) {
    let shown = '';
    const poll = () => {
      if (!session.live) return;
      const report = mod.camera_report();
      // `"none"` before the board boots, `"busy"` while a frame holds the viewer borrowed.
      // Both keep the last reading rather than blanking it: a percentage that flickers to a
      // dash for one frame in the middle of a pinch is worse than one that is a frame stale.
      if (report !== 'none' && report !== 'busy') {
        if (!ready) {
          ready = true;
          for (const control of controls) control.disabled = false;
        }
        const zoom = Number.parseFloat(report);
        if (Number.isFinite(zoom)) {
          const text = percent(zoom);
          // Written only on a change. A pan moves the camera's centre sixty times a second
          // and leaves the zoom alone, so this is the difference between one DOM write per
          // *zoom* and one per frame of every drag.
          if (text !== shown) {
            shown = text;
            readout.textContent = text;
          }
        }
      }
      // ⚠ This adds a callback to a frame the page was already going to run: `schedule_frame`
      // in `lib.rs` chains `request_animation_frame` unconditionally for the life of the page,
      // so the poll wakes nothing that was asleep. That distinction is the whole of why the
      // desktop app refuses a blinking caret and can still afford a live readout here — and
      // rAF stops on its own when the tab is hidden, which a timer would not.
      requestAnimationFrame(poll);
    };
    requestAnimationFrame(poll);
  }

  if (connection) {
    // ⚠ **A timer, not `requestAnimationFrame`, and the difference is the whole point.**
    // rAF stops when the tab is hidden — which is exactly when a board falls behind and
    // exactly what this exists to report. A tab returned to after ten minutes must be able to
    // say "offline" rather than showing whatever it last managed to paint. One second,
    // because the states change a handful of times an hour and a faster poll is a DOM read
    // nobody benefits from.
    let shownState = '';
    // ⚠ **`starting` has to time out, or a board that never loads says "Connecting" for ever.**
    // The viewer is only installed once `boot` succeeds, so a 404, a bad token or a GPU that
    // will not come up all leave `sync_status` answering `starting` permanently. The page's
    // own status line carries the real sentence; what this must not do is keep promising that
    // something is still happening. Fifteen seconds is past a cold start on a slow phone
    // (155ms warm, and the budget for the whole boot is a few seconds) and well short of the
    // point where somebody assumes it is broken.
    // ⚠ **Forty-five seconds, and the trade is stated because it cuts both ways.**
    //
    // Boot is a GPU handshake plus a whole board over the wire. Being *early* here puts a red
    // "Did not load" on a page that is about to work, and somebody who sees that once stops
    // believing the indicator — so the number has to clear an iPad on a bad link, not a
    // laptop on loopback (measured there: 941ms). Being *late* means a 404, a wrong token or
    // a GPU that will not start says "Connecting" for three quarters of a minute.
    //
    // Late is the better failure, because the page's own status line carries the real
    // sentence the whole time — `report()` writes it into `#velm-status` — so nobody is left
    // with no information, only with a bar that is slower to agree.
    //
    // It recovers either way: the branch below resets the clock the moment a real status
    // arrives. A false alarm that corrects itself is still a false alarm.
    const STARTING_PATIENCE_MS = 45000;
    let waitingSince = Date.now();
    const words = () => {
      const line = mod.sync_status();
      // `off` is a page with no server behind it — the static `./board.bin` route. Nothing
      // is wrong, there is simply nothing to report, so the indicator removes itself rather
      // than sitting there grey for ever.
      // Final: a static page with no server behind it. Nothing is wrong and nothing will
      // ever be reported, so the indicator removes itself rather than sitting there grey.
      if (line === 'off') return null;
      // ⚠ Not the same as `off`, and conflating them is what made the indicator never
      // appear: this bar is built before `boot` finishes, so every board reads as having no
      // viewer for the first tick or two.
      if (line === 'starting') {
        return Date.now() - waitingSince > STARTING_PATIENCE_MS
          ? { state: 'offline', word: 'Did not load' }
          : { state: 'live', word: 'Connecting' };
      }
      // Booted: anything later that reads `starting` again would be a fresh wait, not this
      // one. (Nothing does today — the viewer is never uninstalled — and resetting the clock
      // here is what keeps that true if it ever is.)
      waitingSince = Date.now();
      // `busy` means a frame holds the viewer borrowed. Keep the last reading: a state that
      // flickers on a frame boundary is worse than one that is a second stale.
      if (line === 'busy') return undefined;
      if (line.startsWith('live')) return { state: 'live', word: 'Live' };
      if (line.startsWith('connecting')) return { state: 'live', word: 'Connecting' };
      if (line.startsWith('offline')) return { state: 'offline', word: 'Not connected' };
      if (line.startsWith('retrying')) return { state: 'trouble', word: 'Reconnecting' };
      return { state: 'trouble', word: 'Unknown' };
    };
    const tick = () => {
      if (!session.live) return;
      // ⚠ **The whole body is guarded, and the reason is the indicator's own purpose.**
      // A throw from `mod.sync_status()` propagated past the reschedule at the bottom, so
      // nothing restarted the chain and the badge froze on its last state — and if that was
      // "Live", it said Live on a dead connection for the life of the tab, which is precisely
      // the report this exists to prevent. `panic = "abort"` poisons a wasm module, so every
      // export throws afterwards: the case where the page is *most* broken is the case where
      // this was *least* able to say so.
      let next;
      try {
        next = words();
      } catch (error) {
        connection.dataset.state = 'offline';
        connection.textContent = '';
        const word = document.createElement('span');
        word.className = 'velm-chrome-sync-word';
        word.textContent = 'Stopped';
        connection.append(word);
        connection.title = String(error);
        shownState = 'stopped';
        // No reschedule. A module that throws once throws every time, and a timer asking it
        // once a second for the life of the tab is noise on a page that has already said
        // everything it can.
        return;
      }
      if (next === null) {
        // ⚠ **The divider goes with it.** Every group above is conditional and the bar is
        // built by joining them, so a divider left against the bar's own edge is the tell
        // that a group failed to draw — this file says so where the joining happens, and
        // then removed the indicator without it. `previousElementSibling` is that divider by
        // construction: the connection is the last group, so it is always preceded by one.
        const rule = connection.previousElementSibling;
        if (rule && rule.classList.contains('velm-chrome-rule')) rule.remove();
        connection.remove();
        return;
      }
      if (next && next.state + next.word !== shownState) {
        shownState = next.state + next.word;
        connection.dataset.state = next.state;
        // `textContent` on a fresh span, never `innerHTML`: the sentence after the state
        // comes off the wire, and although only the first word is used here, the habit is
        // what keeps that true after the next edit.
        connection.textContent = '';
        const word = document.createElement('span');
        word.className = 'velm-chrome-sync-word';
        word.textContent = next.word;
        connection.append(word);
        // The reason a reader would want, without spending bar width on it.
        connection.title = mod.sync_status();
      }
      session.syncTimer = window.setTimeout(tick, 1000);
    };
    // ⚠ **Scheduled, not called.** Calling it here ran before `insertAdjacentElement` puts
    // the bar in the document — so a throw on the first tick meant **no bar at all**: no
    // Back, no board name, no zoom, on a page that otherwise worked. And `mounted` was
    // already set, so a retry took the already-mounted branch and removed a bar that had
    // never been inserted. A first reading one second late costs nothing; the word starts
    // empty and the element is already sized by its dot.
    session.syncTimer = window.setTimeout(tick, 0);
  }

  // After the canvas, and with no `z-index`. A positioned element paints above the static
  // canvas on DOM order alone, and the two fixed panels declared later in the page — the
  // status line and the board list — then paint above *this*. That is the order worth having:
  // if the list is ever up while the canvas is alive, it covers the bar rather than the bar
  // floating over a list of boards.
  canvas.insertAdjacentElement('afterend', bar);
  return bar;
}
