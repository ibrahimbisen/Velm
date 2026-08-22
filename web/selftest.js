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
    const factor = moved / expected;
    // ⚠ A **band**, not a floor. `moved > expected * 0.6` catches a *missing* device-ratio
    // conversion (which halves the pan on a retina screen) and sails past a *doubled* one,
    // which is the equally likely mistake — the ratio is applied at the call site and could
    // as easily be folded into the contact as well. On a 1x monitor both bugs are invisible,
    // so this fixture is the only thing between them and the iPad they show up on.
    const held = Math.abs(after.zoom - before.zoom) < 1e-9;
    const ok = factor > 0.8 && factor < 1.25 && held;
    findings.push([ok, `one finger dragged right: world x moved ${moved.toFixed(1)} ` +
      `(want ~${expected.toFixed(1)}, ratio ${factor.toFixed(2)}), zoom held ${held}`]);
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
    // Banded for the same reason as finding 1: `dy > 30` against an expected 50 passes on a
    // doubled ratio, which reports 100.
    const ok = held && dy > 40 && dy < 62 && lurch < 20;
    findings.push([ok, `two fingers moved down 50px: board moved ${dy.toFixed(1)}px down ` +
      `(want ~50), lurched ${lurch.toFixed(1)}px sideways mid-gesture (want < 20), ` +
      `zoom held ${held}`]);
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

  // 6. A pinch that scales *and* travels, judged by the world point under the midpoint.
  //
  // ⚠ The only gesture that can tell the composition apart from its plausible mistakes, and
  // none of the five above is one. Findings 2 and 3 are symmetric about a fixed centre, so
  // the pan term is zero; finding 4 translates without scaling, so the zoom term is 1. The
  // difference between "zoom about the old midpoint then pan" and either "pan then zoom" or
  // "zoom about the new midpoint" is exactly `(s-1) × delta` — zero whenever *either* factor
  // is zero, which is every case above. So all five pass on a build with the terms
  // transposed.
  //
  // The assertion is the property the composition exists to provide: the world point under
  // the fingers' midpoint does not move.
  {
    const px = window.devicePixelRatio || 1;
    const from = { a: [cx - 60, cy - 60], b: [cx + 60, cy + 60] };
    const to   = { a: [cx + 40, cy - 160], b: [cx + 280, cy + 80] };
    const midFrom = [(from.a[0] + from.b[0]) / 2, (from.a[1] + from.b[1]) / 2];
    const midTo   = [(to.a[0] + to.b[0]) / 2, (to.a[1] + to.b[1]) / 2];

    const before = camera(mod);
    // The world point under the starting midpoint, computed from the camera rather than
    // asked of it — the client exposes the camera, not a projection.
    const canvasW = canvas.width, canvasH = canvas.height;
    const toWorld = (cam, sx, sy) => [
      cam.x + (sx * px - canvasW / 2) / cam.zoom,
      cam.y + (sy * px - canvasH / 2) / cam.zoom,
    ];
    const anchored = toWorld(before, midFrom[0], midFrom[1]);
    twoFingers(canvas, from, to);
    const after = camera(mod);
    const landed = toWorld(after, midTo[0], midTo[1]);
    const drift = Math.hypot(landed[0] - anchored[0], landed[1] - anchored[1]) * after.zoom / px;
    const scaled = after.zoom / before.zoom;
    const ok = drift < 6 && scaled > 1.2;
    findings.push([ok, `a pinch that scaled ${scaled.toFixed(2)}x while travelling left the ` +
      `world point under the fingers ${drift.toFixed(1)}px from where it started (want < 6)`]);
  }

  const failed = findings.filter(([ok]) => !ok);
  const verdict = failed.length === 0
    ? `touch PASS — ${findings.map(([, m]) => m).join('; ')}`
    : `touch FAIL (${failed.length}/${findings.length}) — ${failed.map(([, m]) => m).join('; ')}`;
  mod.verdict(verdict);
  console.log(verdict);
  return failed.length === 0;
}

/**
 * Frame timing on a fitted board — the number `docs/08-web.md` lists as unmeasured.
 *
 * Measures the interval between animation frames, which is what a person feels, rather than
 * the time inside `frame()`, which omits everything the browser does around it. The **first**
 * frames are reported separately and on purpose: that is when every visible block is shaped
 * and every auto-fitted sticky is binary-searched for its size, and an average over a
 * hundred frames hides exactly the stall a person would notice on opening a board.
 */
export function runPerfFixture(mod, frames = 120) {
  return new Promise((resolve) => {
    const gaps = [];
    let last = performance.now();
    function tick() {
      const now = performance.now();
      gaps.push(now - last);
      last = now;
      if (gaps.length < frames) return requestAnimationFrame(tick);

      // The first gap is the wait for the first frame, not a frame, so it is dropped.
      const measured = gaps.slice(1);
      const sorted = [...measured].sort((a, b) => a - b);
      const at = (q) => sorted[Math.min(sorted.length - 1, Math.floor(sorted.length * q))];
      const opening = measured.slice(0, 10);
      const worstOpening = Math.max(...opening);
      const steady = measured.slice(10);
      const line =
        `perf — ${measured.length} frames: median ${at(0.5).toFixed(1)}ms ` +
        `(${(1000 / at(0.5)).toFixed(0)}fps), 95th ${at(0.95).toFixed(1)}ms, ` +
        `worst ${sorted[sorted.length - 1].toFixed(1)}ms; ` +
        `first 10 frames worst ${worstOpening.toFixed(1)}ms, ` +
        `steady median ${median(steady).toFixed(1)}ms`;
      mod.verdict(line);
      console.log(line);
      resolve(line);
    }
    requestAnimationFrame(tick);
  });
}

function median(xs) {
  if (xs.length === 0) return 0;
  const sorted = [...xs].sort((a, b) => a - b);
  return sorted[Math.floor(sorted.length / 2)];
}
