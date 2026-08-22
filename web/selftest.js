// Gesture fixtures, driven through the page's real event listeners.
//
// Open the board with `?selftest=touch` and this runs. It exists because the whole of
// `input.rs` hangs off DOM listeners: a test that calls a handler directly starts downstream
// of everything that can go wrong between the browser and the handler, which is trap 9 in
// CLAUDE.md restated for a second target. So these dispatch real `PointerEvent`s at the real
// canvas and then ask the wasm module what the camera actually did.
//
// The verdict goes to the status line *and* back to the serving HTTP server, so it is
// readable without a screenshot — on a headless browser, and equally on an iPad the person
// running it is holding.

/** The camera as {zoom, x, y}, read out of wasm. */
function camera(mod) {
  const [zoom, x, y] = mod.camera_report().split(' ').map(Number);
  return { zoom, x, y };
}

function contact(canvas, type, id, x, y) {
  canvas.dispatchEvent(new PointerEvent(type, {
    pointerId: id, pointerType: 'touch', isPrimary: id === 1,
    clientX: x, clientY: y, bubbles: true, cancelable: true,
  }));
}

/** Interpolate a two-finger gesture in `steps`, so it is a gesture and not a teleport. */
function twoFingers(canvas, from, to, steps = 12) {
  contact(canvas, 'pointerdown', 1, from.a[0], from.a[1]);
  contact(canvas, 'pointerdown', 2, from.b[0], from.b[1]);
  for (let i = 1; i <= steps; i++) {
    const t = i / steps;
    const mix = (p, q) => [p[0] + (q[0] - p[0]) * t, p[1] + (q[1] - p[1]) * t];
    // One event moves one finger, which is how a browser actually delivers this — moving
    // both in a single event would exercise a path the hardware never produces.
    const a = mix(from.a, to.a), b = mix(from.b, to.b);
    contact(canvas, 'pointermove', 1, a[0], a[1]);
    contact(canvas, 'pointermove', 2, b[0], b[1]);
  }
  contact(canvas, 'pointerup', 1, to.a[0], to.a[1]);
  contact(canvas, 'pointerup', 2, to.b[0], to.b[1]);
}

function oneFinger(canvas, from, to, steps = 10) {
  contact(canvas, 'pointerdown', 1, from[0], from[1]);
  for (let i = 1; i <= steps; i++) {
    const t = i / steps;
    contact(canvas, 'pointermove', 1,
      from[0] + (to[0] - from[0]) * t, from[1] + (to[1] - from[1]) * t);
  }
  contact(canvas, 'pointerup', 1, to[0], to[1]);
}

export function runTouchFixture(mod, canvas) {
  const findings = [];
  const cx = Math.round(canvas.clientWidth / 2);
  const cy = Math.round(canvas.clientHeight / 2);

  // 1. One finger pans, and pans *with* the finger.
  //
  // The sign is the whole assertion. `pan_by_screen_delta` subtracts internally, so an extra
  // negation in the handler makes the board run away from the finger — which is exactly what
  // shipped for the mouse, and which a pixel counter reported as a perfectly good frame.
  {
    const before = camera(mod);
    oneFinger(canvas, [cx - 150, cy], [cx + 150, cy]);
    const after = camera(mod);
    const moved = before.x - after.x;
    const expected = 300 * (window.devicePixelRatio || 1) / after.zoom;
    const ok = moved > expected * 0.6 && Math.abs(after.zoom - before.zoom) < 1e-9;
    findings.push([ok, `one finger dragged right: world x moved ${moved.toFixed(1)} ` +
      `(want ~${expected.toFixed(1)}, same sign), zoom held ${after.zoom === before.zoom}`]);
  }

  // 2. Two fingers spreading zoom in, by the ratio of their separation.
  //
  // Asserted as a *ratio* rather than "zoom went up", because a build that zooms by the
  // wrong law still zooms up — and the wrong law is the likely mistake here, not the wrong
  // direction. Fingers 100px apart taken to 300px apart is exactly 3x.
  {
    const before = camera(mod);
    twoFingers(canvas,
      { a: [cx - 50, cy], b: [cx + 50, cy] },
      { a: [cx - 150, cy], b: [cx + 150, cy] });
    const after = camera(mod);
    const got = after.zoom / before.zoom;
    const ok = Math.abs(got - 3) < 0.15;
    findings.push([ok, `pinch out 100px -> 300px zoomed ${got.toFixed(3)}x (want 3.000x)`]);
  }

  // 3. Pinching back in returns to where it started.
  //
  // The composition property the exponential and the ratio both have, and the reason the
  // factor is a ratio at all. A build that adds pixel deltas fails this and passes 2.
  {
    const before = camera(mod);
    twoFingers(canvas,
      { a: [cx - 150, cy], b: [cx + 150, cy] },
      { a: [cx - 50, cy], b: [cx + 50, cy] });
    const after = camera(mod);
    const got = after.zoom / before.zoom;
    const ok = Math.abs(got - 1 / 3) < 0.02;
    findings.push([ok, `pinch back in returned ${got.toFixed(4)}x (want 0.3333x)`]);
  }

  // 4. Two fingers travelling together pan smoothly — checked *mid-gesture*.
  //
  // ⚠ The end-to-end pan is deliberately not the assertion, and finding that out is what
  // this fixture is for. A shared single drag slot cancels out over a matched pair of moves,
  // so the board lands in exactly the right place having lurched half the fingers' separation
  // sideways and back on every event in between. Sampling *between* the two fingers' moves is
  // the only way to see it, and it is what a person on a tablet sees: a board that shakes.
  {
    const before = camera(mod);
    contact(canvas, 'pointerdown', 1, cx - 100, cy - 60);
    contact(canvas, 'pointerdown', 2, cx + 100, cy - 60);
    contact(canvas, 'pointermove', 1, cx - 100, cy - 10);
    const half = camera(mod);
    contact(canvas, 'pointermove', 2, cx + 100, cy - 10);
    const after = camera(mod);
    contact(canvas, 'pointerup', 1, cx - 100, cy - 10);
    contact(canvas, 'pointerup', 2, cx + 100, cy - 10);

    const px = window.devicePixelRatio || 1;
    const lurch = Math.abs(half.x - before.x) * before.zoom / px;
    const dy = (before.y - after.y) * after.zoom / px;
    const held = Math.abs(after.zoom / before.zoom - 1) < 0.06;
    const ok = held && dy > 30 && lurch < 20;
    findings.push([ok, `two fingers moved down 50px: board moved ${dy.toFixed(1)}px down, ` +
      `lurched ${lurch.toFixed(1)}px sideways mid-gesture (want < 20), zoom held ${held}`]);
  }

  // 5. A second finger landing mid-drag must not throw the board.
  //
  // The case the other four all pass without: the old single-slot drag state let a second
  // `pointerdown` overwrite the first finger's last position, so the very next move computed
  // a delta against the *other* finger and the board jumped by their separation.
  {
    contact(canvas, 'pointerdown', 1, cx - 100, cy);
    contact(canvas, 'pointermove', 1, cx - 60, cy);
    const before = camera(mod);
    contact(canvas, 'pointerdown', 2, cx + 200, cy + 120);
    // ⚠ The finger that moves next is **finger one**, and that is the whole assertion.
    // Moving finger two would compute a delta against finger two's own landing point on
    // either build and pass on both — the shared slot only bites when the *other* finger
    // moves after it has been overwritten.
    contact(canvas, 'pointermove', 1, cx - 58, cy);
    const after = camera(mod);
    contact(canvas, 'pointerup', 1, cx - 58, cy);
    contact(canvas, 'pointerup', 2, cx + 200, cy + 120);
    const jump = Math.hypot(after.x - before.x, after.y - before.y) * after.zoom;
    const ok = jump < 20;
    findings.push([ok, `a second finger landing mid-drag moved the board ${jump.toFixed(1)}px ` +
      `on screen (want < 20)`]);
  }

  const failed = findings.filter(([ok]) => !ok);
  const verdict = failed.length === 0
    ? `touch PASS — ${findings.map(([, m]) => m).join('; ')}`
    : `touch FAIL (${failed.length}/${findings.length}) — ${failed.map(([, m]) => m).join('; ')}`;
  mod.verdict(verdict);
  console.log(verdict);
  return failed.length === 0;
}
