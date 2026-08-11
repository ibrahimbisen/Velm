# Mouse controls

**Scope: mouse only.** Trackpad gestures are explicitly deferred at the user's request and must not be worked on until they say so. Where this document and trackpad behaviour disagree, this document wins for now.

The user's boards are technical diagrams built over years in Miro. Muscle memory is the whole point — every binding below matches Miro unless a reason is given.

---

## 1. Bindings

| Input | Action |
|---|---|
| **Left click** on an item | Select it (topmost by z-order) |
| **Left click** on empty canvas | Clear the selection |
| **Shift + left click** | Add to / remove from the selection |
| **Left drag** from empty canvas | **Marquee select** — a rectangle; items intersecting it are selected |
| **Left drag** from an item | Move the selection |
| **Middle drag** | **Pan** |
| **Space held + left drag** | **Pan** — cursor becomes a grabbing hand |
| **Right drag** | **Pan** — a convenience many diagram tools offer; costs nothing and helps mice without a usable middle button |
| **Scroll wheel** | **Zoom, anchored at the cursor** — see the note below |
| **Shift + scroll wheel** | Pan horizontally |
| **⌘ / Ctrl + scroll wheel** | **Zoom, anchored at the cursor** |
| **Right click** | Context menu (once the chrome is wired; until then, no-op) |
| **Double click** an item | Enter text editing |
| **Double click** empty canvas | Create a sticky note at that point |

**Left drag must never pan.** That was the single worst fault in the first version: it stole the gesture marquee selection needs, so there was no way to select several items.

**Selection happens on press, not on release.** It is the only order in which an item can be grabbed and dragged in one gesture. A press inside an existing multi-selection keeps that selection, so several items move together; replacing it with whatever was under the pointer is how you make a careful selection impossible to move.

**Corrected 2026-07-28 — the wheel zooms.** This table used to say the bare wheel panned vertically, and the build has always zoomed. The build is right and this document was wrong, and it is recorded here rather than quietly edited because the header says this document wins: a mouse wheel has one axis and coarse notches, so panning with it is slow and can only go one direction at a time, which is the specific complaint that started this. Zooming is what CAD and diagram tools bind it to, `Shift` still pans sideways, and a trackpad's two-finger scroll still pans because a `PixelDelta` and a `LineDelta` are told apart. `--wheel-pans` restores the old binding for anyone who wants it. `CLAUDE.md` and `crates/vellum-app/src/input.rs` agree with this table.

## 2. Zoom

- **Anchored at the cursor.** The world point under the pointer must be at the same screen position after the zoom as before it. This is a hard invariant and is unit-tested over a thousand consecutive steps for drift.
- **Multiplicative, not additive.** Each notch multiplies by a constant factor, so zooming feels the same at 10% as at 1000%. Additive steps feel fine at one end of the range and unusable at the other.
- **Wheel notches are coarse.** A mouse wheel emits discrete `LineDelta` events, typically ±1 per notch, unlike a trackpad's fine pixel deltas. Roughly **1.15× per notch** — enough to feel responsive, small enough to land on a value you wanted. Do not reuse the trackpad's per-pixel rate here; it makes a wheel either uselessly slow or wildly overshooting.
- **Clamp** to 1%–6400%.
- `0` resets to 100% about the viewport centre; `F` fits the board.

## 3. Wheel panning

- One notch pans a fixed number of **screen** pixels, not world units, so the felt distance is the same at every zoom. About 60px per notch.
- Honour the OS "natural scrolling" preference: read it rather than assuming a direction. Getting the sign wrong makes the canvas fight the user, which is exactly what the first version did.
- Provide an inversion toggle regardless — preferences differ from the OS setting more often than you would expect.

## 4. Feel

These are what separate "works" from "feels right", and none of them can be verified by a test:

- **No acceleration curves on the wheel.** A notch is a notch. Acceleration belongs to trackpads.
- **Pan is 1:1 with the pointer** while dragging. The point grabbed stays under the cursor exactly. No smoothing, no lag, no easing — any of them read as latency.
- **No inertia on a mouse.** Wheel and drag stop when the input stops. Momentum is a trackpad idiom and feels broken with a mouse.
- **Cursor communicates mode**: default arrow, open hand when space is held, grabbing hand while panning, crosshair for a marquee, move cursor over a draggable item.
- **A drag has a threshold** before it counts as a drag. Without it, a click with a twitchy hand becomes a 1px move and pollutes the undo stack. One number serves the marquee and the move — `crates/vellum-app/src/input.rs`'s `CLICK_SLOP`, five **physical** pixels, which is 2.5 points on the Retina display this is built against and therefore the "~3px" this originally asked for. Two separate thresholds would let a drag that had begun moving an item still end as a click.
- **One drag is one undo step**, not one per mouse-move event. A drag writes *nothing* to the document while it runs — only the spatial index moves, so the item is drawn and hit-tested where it is being dragged to — and the whole move is written once as a single undo group when the button comes up. The same rule now covers the properties panel's spinners and sliders, which report a change every frame they are held: those fold into one step by the undo manager's merge interval.
- **Escape abandons a drag** and puts everything back. With nothing being dragged it clears the selection, as it does in Miro.
- **Arrow keys nudge** the selection by one board unit, ten with Shift. One press is one undo step.

## 5. Precision

- Cursor positions arrive from winit in **physical** pixels. On a Retina display `scale_factor` is 2.0. Every screen-space path must be consistently physical or consistently logical — mixing them makes zoom-at-cursor drift by exactly a factor of two, which reads as "the zoom is broken".
- Convert to `f64` world space through the camera, never by scaling raw deltas.

## 6. Explicitly out of scope

Two-finger scroll, pinch, momentum, rubber-banding, and any `PinchGesture` handling. The user will ask for these separately. Existing trackpad code should be left in place and simply not tuned — do not delete it, do not "improve" it.
