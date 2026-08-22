//! Pointer, touch and wheel input, translated to camera moves.
//!
//! Deliberately small, and deliberately not a port of `vellum_app::input`. That module owns a
//! gesture machine, a velocity estimator, a double-click window and the `resolve_press` slot
//! that trap 5 in `CLAUDE.md` is about — none of which a read-only viewer has any use for,
//! because there is nothing here to select, drag or place.
//!
//! What it does keep is the two bindings the hands already know, from
//! `docs/06-mouse-controls.md`: **the wheel zooms about the pointer**, and **dragging pans**.
//! On a touchscreen those become one finger pans and two fingers pinch — the bindings every
//! map on every phone has, which is the only convention worth matching on a tablet.
//!
//! # ⚠ Pointer events only. Never `TouchEvent`.
//!
//! On iOS a finger fires *both* families: `touchstart`/`touchmove` **and** a compatibility
//! `pointerdown`/`pointermove`. So a `touchmove` pinch handler layered on top of the pointer
//! handlers below does not add pinching to panning — it runs the two at once, and the board
//! zooms and slides at the same time from one gesture. `PointerEvent` carries `pointerId`,
//! which is everything a multi-touch gesture needs, so there is nothing to gain by adding the
//! second family and a real defect to gain by trying.
//!
//! # What a finger cannot do here, stated rather than implied
//!
//! **There is no long-press.** A long press means "open the context menu" everywhere else in
//! Velm, and this viewer has no context menu because it has no verbs — nothing to copy, lock,
//! delete or restyle. Wiring the gesture to nothing would be the failure this repository
//! names as worse than a missing feature: a gesture the interface invites and does not
//! answer. When editing arrives, this is where it goes.

use std::cell::RefCell;
use std::rc::Rc;

use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;

use vellum_scene::ScreenPoint;

use crate::Viewer;

/// The fingers (or the one mouse button) currently down, in the order they landed.
///
/// A `Vec` rather than a `HashMap` because it never holds more than a hand, and because the
/// **order matters**: a pinch is decided by the first two contacts, so a third finger landing
/// mid-gesture must not be able to change which two are driving it.
///
/// ⚠ This replaces a single `Option<(f64, f64)>`, and that is the whole of why two fingers
/// used to behave so strangely: a second `pointerdown` overwrote the first finger's last
/// position, and then *both* fingers' moves computed a delta against that one shared slot.
#[derive(Default)]
struct Contacts {
    down: Vec<Contact>,
    /// Whether this gesture has ever had two fingers on it.
    ///
    /// ⚠ **Latched, and it has to be.** A tap is decided at `pointerup`, and by the time the
    /// *second* finger of a pinch lifts there is one contact left which may have travelled
    /// almost nowhere — so a pinch would end by opening whatever card was under that finger.
    /// Cleared when the last finger goes, not when the count drops to one.
    ever_multi: bool,
}

#[derive(Clone, Copy)]
struct Contact {
    id: i32,
    /// CSS pixels, exactly as the event reports them. Converted at the point of use, by
    /// [`ratio`], and nowhere else.
    x: f64,
    y: f64,
    /// Where this finger landed, so its net displacement can be measured.
    start_x: f64,
    start_y: f64,
    /// How far it has travelled since, summed over every move above the jitter floor.
    travelled: f64,
    /// How far it currently is from where it landed.
    ///
    /// ⚠ **Both this and [`Self::travelled`], because either alone is defeatable** — and each
    /// by the gesture the other catches. See the update path for the arithmetic; the short
    /// version is that a drag that returns to its start has no displacement, and a drag slow
    /// enough never clears the per-sample floor.
    drift: f64,
}

/// How far a finger or a cursor may travel and still count as a tap. CSS pixels.
///
/// Generous, because the target is a finger's: `draw.rs`'s own `CLICK_SLOP` exists for the
/// same decision natively, and a value tight enough for a mouse makes the badge unpressable
/// on the device this whole client is for.
const TAP_SLOP: f64 = 6.0;

/// CSS pixels to physical pixels — trap 4, in one place so it cannot be applied twice or
/// forgotten once.
///
/// Every `clientX`/`clientY` off a DOM event is a **CSS** pixel and every coordinate the
/// camera takes is a **physical** one. A gesture that skips the conversion drifts by exactly
/// the device ratio: half speed on a retina iPad, and *exactly correct* on a 1x monitor —
/// so it is a bug that cannot be reproduced on the machine most likely to be testing it.
fn ratio() -> f64 {
    web_sys::window().map_or(1.0, |w| w.device_pixel_ratio()).max(1.0)
}

impl Contacts {
    /// A new contact, from `pointerdown` and nowhere else.
    fn press(&mut self, id: i32, x: f64, y: f64) {
        if !self.moved(id, x, y) {
            self.down.push(Contact { id, x, y, start_x: x, start_y: y, travelled: 0.0, drift: 0.0 });
            if self.down.len() >= 2 {
                self.ever_multi = true;
            }
        }
    }

    /// Update a contact that is already down. Answers whether there was one.
    ///
    /// ⚠ **Update-only, and that is the whole of this function's reason to exist.** It used
    /// to be one insert-or-update called from both handlers, so a `pointermove` from a device
    /// that had never pressed — a hovering Apple Pencil, a trackpad cursor on an iPad, a mouse
    /// over a touchscreen — pushed a phantom contact. Two fingers were then "down", so the
    /// next real move took the pinch arm and computed a ratio against a point that never
    /// moves; and because only a matching `pointerup` removes a contact and a hovering device
    /// never sends one, the phantom outlived the gesture and the board panned with the bare
    /// cursor until the page was reloaded.
    fn moved(&mut self, id: i32, x: f64, y: f64) -> bool {
        match self.down.iter_mut().find(|c| c.id == id) {
            Some(existing) => {
                // ⚠ **Only movement worth calling movement.** Path length is monotonic and
                // never decays, so counting every sample means a finger *resting* accumulates
                // digitiser noise until it can no longer be a tap: at a tenth of a pixel per
                // sample at 60Hz, a deliberate slow press dies after about a second. A
                // careful press is exactly how somebody aims at a small badge.
                //
                // A per-sample floor rather than a decay: a real drag moves further than this
                // between samples and is counted in full, while a hand trying to stay put
                // contributes nothing.
                //
                // ⚠ **On its own the floor is the third mirror of the same bug**, and that is
                // why the release tests two things rather than one. Displacement alone could
                // be defeated by wandering back to the start; floored path length alone can
                // be defeated by moving *slowly* — a pan under about 45 px/s never exceeds
                // the floor on any sample, accumulates nothing, and ends as a tap. Neither
                // measure is sufficient; both together are, because a real drag fails at
                // least one and a real tap passes both.
                const JITTER: f64 = 0.75;
                let step = (x - existing.x).hypot(y - existing.y);
                if step > JITTER {
                    existing.travelled += step;
                }
                existing.drift = (x - existing.start_x).hypot(y - existing.start_y);
                existing.x = x;
                existing.y = y;
                true
            }
            None => false,
        }
    }

    fn remove(&mut self, id: i32) {
        self.down.retain(|c| c.id != id);
        if self.down.is_empty() {
            self.ever_multi = false;
        }
    }

    /// Remove a contact and report it, with whether this gesture was ever multi-touch.
    fn lift(&mut self, id: i32) -> Option<(Contact, bool)> {
        let index = self.down.iter().position(|c| c.id == id)?;
        let contact = self.down.remove(index);
        let multi = self.ever_multi;
        if self.down.is_empty() {
            self.ever_multi = false;
        }
        Some((contact, multi))
    }

    /// Midpoint and separation of the two driving contacts, in CSS pixels.
    fn pinch(&self) -> Option<((f64, f64), f64)> {
        let (a, b) = (self.down.first()?, self.down.get(1)?);
        let mid = ((a.x + b.x) / 2.0, (a.y + b.y) / 2.0);
        Some((mid, (a.x - b.x).hypot(a.y - b.y)))
    }

    fn only(&self) -> Option<Contact> {
        (self.down.len() == 1).then(|| self.down[0])
    }
}

/// Attach the listeners. They live for the life of the page, which is why each closure is
/// `forget`-ed rather than dropped — dropping one unregisters the handler and the canvas
/// silently stops responding.
pub fn attach(canvas: &web_sys::HtmlCanvasElement, viewer: Rc<RefCell<Viewer>>) {
    let contacts = Rc::new(RefCell::new(Contacts::default()));

    {
        let contacts = Rc::clone(&contacts);
        let viewer = Rc::clone(&viewer);
        let target = canvas.clone();
        let handler = Closure::<dyn FnMut(web_sys::PointerEvent)>::new(
            move |event: web_sys::PointerEvent| {
                // Capture, so a finger that slides off the canvas mid-gesture keeps
                // delivering here rather than silently stopping — and so the matching
                // `pointerup` still arrives, which is what actually clears the contact. It
                // is also why `pointerleave` is *not* one of the enders below: with capture
                // it does not fire, and without capture it fires on a mouse the moment the
                // drag reaches the window edge.
                let _ = target.set_pointer_capture(event.pointer_id());
                let single = contacts.borrow().down.is_empty();
                contacts.borrow_mut().press(
                    event.pointer_id(),
                    event.client_x() as f64,
                    event.client_y() as f64,
                );
                // ⚠ **The edit layer is offered the press, and only for the first finger.**
                //
                // A second finger is a pinch, and a pinch that also picked something up would
                // move an item and the camera at once. `crate::edit` answers whether it took
                // the gesture; the move handler reads the same answer rather than guessing,
                // which is what stops a drag being both a move and a pan.
                //
                // Left button only, for `pointerup`'s reason: a right press is a menu, not a
                // grab.
                if single && event.button() == 0 {
                    let ratio = ratio();
                    if let Ok(mut viewer) = viewer.try_borrow_mut() {
                        let at = viewer.camera.screen_to_world(ScreenPoint::new(
                            event.client_x() as f64 * ratio,
                            event.client_y() as f64 * ratio,
                        ));
                        // ⇧ adds to the selection rather than replacing it.
                        crate::edit::pointer_down(&mut viewer, at, event.shift_key());
                    }
                }
            },
        );
        canvas
            .add_event_listener_with_callback("pointerdown", handler.as_ref().unchecked_ref())
            .ok();
        handler.forget();
    }

    {
        let viewer = Rc::clone(&viewer);
        let contacts = Rc::clone(&contacts);
        let handler = Closure::<dyn FnMut(web_sys::PointerEvent)>::new(
            move |event: web_sys::PointerEvent| {
                let mut contacts = contacts.borrow_mut();
                if contacts.down.is_empty() {
                    return;
                }
                // The state *before* this move is the baseline, so it is read first. One
                // event moves exactly one contact, and the gesture is the difference the two
                // readings make — which is what lets a pinch be incremental with no separate
                // "gesture start" to latch, and no jump when a third finger lands or lifts.
                let before_one = contacts.only();
                let before_pinch = contacts.pinch();
                // A move for a pointer that never pressed is not a gesture. Ignored, rather
                // than admitted as a contact — see `Contacts::moved`.
                if !contacts.moved(
                    event.pointer_id(),
                    event.client_x() as f64,
                    event.client_y() as f64,
                ) {
                    return;
                }
                let after_pinch = contacts.pinch();

                let ratio = ratio();
                let Ok(mut viewer) = viewer.try_borrow_mut() else { return };

                match (before_pinch, after_pinch) {
                    (Some((mid, was)), Some((now_mid, is))) => {
                        // A second finger means this is a pinch, not a drag. Anything picked
                        // up is put back rather than carried along — moving an item and the
                        // camera at once is the shape `Input::cancel_gesture` exists for on
                        // the desktop.
                        crate::edit::pointer_cancel(&mut viewer);
                        // ⚠ **The factor is a ratio of separations, not an exponential of a
                        // pixel delta.** `zoom_by` multiplies, so a ratio is already the
                        // right shape and it is scale-invariant — the CSS-to-physical
                        // conversion cancels out of it entirely, which is why `ratio` is
                        // applied to the midpoint below and to nothing here. Feeding finger
                        // *pixels* through the wheel's per-pixel exponential would detach the
                        // board from the fingers, because the correct factor is relative to
                        // the current scale and that one is not.
                        //
                        // `was > PINCH_FLOOR` rather than `> 0.0`: two contacts a hair apart
                        // give an enormous ratio from a one-pixel move, and `zoom_by` refuses
                        // a non-finite factor but happily accepts a merely absurd one.
                        // Both ends, not just the denominator. A floor on `was` alone let a
                        // pinch drive the zoom to `MIN_ZOOM`, where `zoom_by` clamps and the
                        // truncated amount is forgotten — so spreading the fingers back to
                        // where they started left the board substantially *more* zoomed in
                        // than before, and repeating the gesture walked it further each time.
                        // A board opens fitted at about 4%, four times the floor, so an
                        // ordinary pinch-out reaches it inside one gesture.
                        const PINCH_FLOOR: f64 = 8.0;
                        if was > PINCH_FLOOR && is > PINCH_FLOOR {
                            let anchor =
                                ScreenPoint::new(mid.0 * ratio, mid.1 * ratio);
                            viewer.camera.zoom_by(is / was, anchor);
                        }
                        // Then the pan, because a pinch that also travels is two motions.
                        // `zoom_by` pins the **old** midpoint exactly, so the natural
                        // composition is: zoom about where the fingers were, then move the
                        // board by however far their centre went.
                        viewer.camera.pan_by_screen_delta(
                            (now_mid.0 - mid.0) * ratio,
                            (now_mid.1 - mid.1) * ratio,
                        );
                    }
                    _ => {
                        // ⚠ **The edit layer first.** If a gesture picked something up, this
                        // motion moves *it*, not the board — and `pointer_move` answers
                        // whether it took the motion, so the two can never both happen. It
                        // answers `false` when nothing is being dragged, which is the common
                        // case and costs one compare.
                        {
                            let at = viewer.camera.screen_to_world(ScreenPoint::new(
                                event.client_x() as f64 * ratio,
                                event.client_y() as f64 * ratio,
                            ));
                            if crate::edit::pointer_move(&mut viewer, at) {
                                return;
                            }
                        }
                        // One contact: a plain drag.
                        //
                        // ⚠ Not negated. `Camera::pan_by_screen_delta` already does
                        // `center -= dx / zoom`, so handing it the raw pointer delta is what
                        // makes the board travel *with* the finger. Negating here inverts it
                        // twice and the board runs away from the pointer — which is exactly
                        // what shipped, and what the user caught in thirty seconds of
                        // dragging after the pixel-counter had happily reported the frame as
                        // correct. An assertion about pixels cannot see a sign error in a
                        // gesture.
                        let Some(from) = before_one else { return };
                        if from.id != event.pointer_id() {
                            return;
                        }
                        viewer.camera.pan_by_screen_delta(
                            (event.client_x() as f64 - from.x) * ratio,
                            (event.client_y() as f64 - from.y) * ratio,
                        );
                    }
                }
            },
        );
        canvas
            .add_event_listener_with_callback("pointermove", handler.as_ref().unchecked_ref())
            .ok();
        handler.forget();
    }

    // ⚠ **`pointerup` and `pointercancel` are two handlers now, not one loop.** They used to
    // share one, which was right while a release only ended a gesture. A release can open a
    // link card's page; a *cancel* — the browser taking the pointer away, a system gesture,
    // the page being hidden — must never do that. Sharing the handler would make an
    // interruption indistinguishable from a deliberate tap.
    {
        let viewer = Rc::clone(&viewer);
        let contacts = Rc::clone(&contacts);
        let handler = Closure::<dyn FnMut(web_sys::PointerEvent)>::new(
            move |event: web_sys::PointerEvent| {
                // Left button only. A right-click is a press *and* a release that passes the
                // slop test, so without this the context menu and the page would both open.
                if event.button() != 0 {
                    contacts.borrow_mut().remove(event.pointer_id());
                    return;
                }
                let lifted = contacts.borrow_mut().lift(event.pointer_id());
                let Some((contact, was_multi)) = lifted else { return };
                // ⚠ **The edit layer is asked first, and a completed move is not a tap.**
                // Without this a drag that finishes over a link card would move the item *and*
                // open its page — and `pointer_up` is also the only thing that commits, so a
                // `return` above it would leave a preview drawn against a document that never
                // changed.
                {
                    let Ok(mut viewer) = viewer.try_borrow_mut() else { return };
                    if crate::edit::pointer_up(&mut viewer) {
                        return;
                    }
                }
                if was_multi {
                    return;
                }
                // A pan that finishes over a card is not a request to open it.
                // ⚠ **Both measures, and a drag fails at least one.** A pan that wanders
                // out and back has almost no *displacement*, so a straight-line test calls it
                // a tap; a pan slow enough never clears the jitter floor, so *path length*
                // calls it one too. The `Contact` fields carry the full argument.
                if contact.travelled > TAP_SLOP || contact.drift > TAP_SLOP {
                    return;
                }
                let ratio = ratio();
                let url = {
                    // Scoped, so the borrow is over before the window is asked to navigate.
                    let Ok(viewer) = viewer.try_borrow() else { return };
                    let world = viewer
                        .camera
                        .screen_to_world(ScreenPoint::new(contact.x * ratio, contact.y * ratio));
                    // ⚠ **`hit_test_where`, not `hit_test` then filter** — and `scene.rs`
                    // states the rule verbatim: *"The predicate is applied before 'topmost',
                    // not after… filtering afterwards turns a rejected item into a hole in
                    // the board."* Filtered after, a clipped card lying over a visible one
                    // does not merely fail to open — it **swallows the tap**, and the card
                    // underneath, which is drawn and does have an address, never gets it.
                    //
                    // The guard itself is the one both paint passes apply. `Scene::hit_test`
                    // knows nothing about frames, so without it a tap on visually empty board
                    // opens the page of a card that is not drawn there — reachable, because a
                    // frame *resized* past its children on the Mac leaves them clipped and
                    // persisted, and opening a page is the only verb this viewer has.
                    //
                    // Native reaches for the same function for the same reason, at two sites
                    // in `actions.rs`, where the predicate is the lock set.
                    viewer
                        .projection
                        .scene()
                        .hit_test_where(world, |id| {
                            viewer.projection.get(id).is_some_and(|projected| {
                                !vellum_project::frame::clipped_by_frame(
                                    projected,
                                    &viewer.projection,
                                )
                            })
                        })
                        .and_then(|id| viewer.projection.get(id))
                        .and_then(|projected| crate::badges::pressed(projected, world))
                };
                // ⚠ **Opened here, synchronously inside the handler.** `window.open` needs
                // the transient activation a real `pointerup` grants; deferred to the next
                // frame it is a popup and every browser refuses it silently — which would be
                // a badge that draws, hit-tests, reports success and does nothing.
                if let Some(url) = url {
                    crate::badges::open_in_new_tab(&url);
                }
            },
        );
        canvas
            .add_event_listener_with_callback("pointerup", handler.as_ref().unchecked_ref())
            .ok();
        handler.forget();
    }
    {
        let contacts = Rc::clone(&contacts);
        let viewer = Rc::clone(&viewer);
        let handler = Closure::<dyn FnMut(web_sys::PointerEvent)>::new(
            move |event: web_sys::PointerEvent| {
                contacts.borrow_mut().remove(event.pointer_id());
                // ⚠ **Feedback 27's rule: give every way a gesture can end without a release
                // a call to the function that closes it.** This repository has paid for that
                // on four separate gestures. A cancelled drag whose preview is never put back
                // leaves items drawn where they are not, against a document that never
                // changed — and `pointercancel` is routine on a touchscreen.
                if let Ok(mut viewer) = viewer.try_borrow_mut() {
                    crate::edit::pointer_cancel(&mut viewer);
                }
            },
        );
        canvas
            .add_event_listener_with_callback("pointercancel", handler.as_ref().unchecked_ref())
            .ok();
        handler.forget();
    }

    {
        let viewer = Rc::clone(&viewer);
        let handler =
            Closure::<dyn FnMut(web_sys::WheelEvent)>::new(move |event: web_sys::WheelEvent| {
                // Without this the page scrolls behind the board, which on a trackpad makes
                // the canvas feel like it is sliding away from you.
                event.prevent_default();
                let ratio = ratio();
                let anchor = ScreenPoint::new(
                    event.client_x() as f64 * ratio,
                    event.client_y() as f64 * ratio,
                );
                // Exponential in the wheel delta, so zooming in and back out returns exactly
                // where it started — the property `vellum_app::input` argues for at length.
                //
                // ⚠ The **rate** is deliberately not native's `ZOOM_RATE_PER_PIXEL` of 0.005,
                // and an older comment here claiming parity was simply wrong. Native applies
                // its constant to a macOS scroll delta; a browser reports a wheel notch as a
                // much larger number, so the same constant is roughly twice as fast in a tab.
                // This value is the one that has been used and reported as feeling right, and
                // "make it agree with a constant in another module" is not a reason to change
                // a number a person has tested.
                const ZOOM_RATE_PER_PIXEL: f64 = 0.0025;
                let factor = (-event.delta_y() * ZOOM_RATE_PER_PIXEL).exp();
                if let Ok(mut viewer) = viewer.try_borrow_mut() {
                    viewer.camera.zoom_by(factor, anchor);
                }
            });
        // ⚠ Attached without `{passive: false}`, which the `AddEventListenerOptions` binding
        // would need an extra web-sys feature for. On a canvas element Chrome and Safari
        // treat a wheel listener as non-passive by default, so `prevent_default` above still
        // holds -- but this is a browser default rather than a guarantee, and if the page is
        // ever seen scrolling behind the board, this is the line to fix.
        canvas
            .add_event_listener_with_callback("wheel", handler.as_ref().unchecked_ref())
            .ok();
        handler.forget();
    }

    // A resized window has to reach the surface *and* the camera, or the board is drawn at
    // the old aspect ratio and every screen-to-world conversion is wrong with it.
    //
    // `resize` **and** `orientationchange`: rotating an iPad fires the second reliably and
    // the first only sometimes, and a board drawn at the old aspect after a rotation is the
    // most obvious possible bug on the device this port exists for.
    {
        let viewer = Rc::clone(&viewer);
        let canvas = canvas.clone();
        let handler = Closure::<dyn FnMut()>::new(move || {
            let Some(window) = web_sys::window() else { return };
            let (w, h) = crate::size_of(&canvas, &window);
            if let Ok(mut viewer) = viewer.try_borrow_mut() {
                viewer.resize(w, h);
            }
        });
        if let Some(window) = web_sys::window() {
            for event in ["resize", "orientationchange"] {
                window
                    .add_event_listener_with_callback(event, handler.as_ref().unchecked_ref())
                    .ok();
            }
        }
        handler.forget();
    }
}
