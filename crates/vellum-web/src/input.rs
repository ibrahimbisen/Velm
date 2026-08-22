//! Pointer and wheel input, translated to camera moves.
//!
//! Deliberately small, and deliberately not a port of `vellum_app::input`. That module owns a
//! gesture machine, a velocity estimator, a double-click window and the `resolve_press` slot
//! that trap 5 in `CLAUDE.md` is about — none of which a read-only viewer has any use for,
//! because there is nothing here to select, drag or place.
//!
//! What it does keep is the two bindings the hands already know, from
//! `docs/06-mouse-controls.md`: **the wheel zooms about the pointer**, and **dragging pans**.
//!
//! ⚠ **There is no touch handling here, and that is the honest state of the port.** winit's
//! web backend emits `WindowEvent::Touch` and nothing else for a finger, and Velm has never
//! had a touch path on any target. Pointer events do give us `pointerType: "touch"` for a
//! single finger, so a one-finger drag pans on an iPad — but pinch-to-zoom, two-finger pan
//! and long-press are not built, and pretending otherwise would be the one failure this
//! repository names as worse than a missing feature: describing a gesture the user cannot
//! perform.

use std::cell::RefCell;
use std::rc::Rc;

use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;

use vellum_scene::ScreenPoint;

use crate::Viewer;

/// Attach the listeners. They live for the life of the page, which is why each closure is
/// `forget`-ed rather than dropped — dropping one unregisters the handler and the canvas
/// silently stops responding.
pub fn attach(canvas: &web_sys::HtmlCanvasElement, viewer: Rc<RefCell<Viewer>>) {
    let dragging = Rc::new(RefCell::new(None::<(f64, f64)>));

    {
        let viewer = Rc::clone(&viewer);
        let dragging = Rc::clone(&dragging);
        let handler = Closure::<dyn FnMut(web_sys::PointerEvent)>::new(
            move |event: web_sys::PointerEvent| {
                *dragging.borrow_mut() = Some((event.client_x() as f64, event.client_y() as f64));
                let _ = viewer; // borrowed only to keep the Rc alive alongside the drag state
            },
        );
        canvas
            .add_event_listener_with_callback("pointerdown", handler.as_ref().unchecked_ref())
            .ok();
        handler.forget();
    }

    {
        let viewer = Rc::clone(&viewer);
        let dragging = Rc::clone(&dragging);
        let handler = Closure::<dyn FnMut(web_sys::PointerEvent)>::new(
            move |event: web_sys::PointerEvent| {
                let mut last = dragging.borrow_mut();
                let Some((x, y)) = *last else { return };
                let (nx, ny) = (event.client_x() as f64, event.client_y() as f64);
                *last = Some((nx, ny));
                // CSS pixels from the event, physical pixels in the camera -- trap 4. The
                // ratio has to be applied or a pan drifts by exactly the device scale.
                let ratio = web_sys::window().map_or(1.0, |w| w.device_pixel_ratio()).max(1.0);
                if let Ok(mut viewer) = viewer.try_borrow_mut() {
                    viewer
                        .camera
                        .pan_by_screen_delta(-(nx - x) * ratio, -(ny - y) * ratio);
                }
            },
        );
        canvas
            .add_event_listener_with_callback("pointermove", handler.as_ref().unchecked_ref())
            .ok();
        handler.forget();
    }

    for end in ["pointerup", "pointercancel", "pointerleave"] {
        let dragging = Rc::clone(&dragging);
        let handler = Closure::<dyn FnMut(web_sys::PointerEvent)>::new(
            move |_: web_sys::PointerEvent| {
                *dragging.borrow_mut() = None;
            },
        );
        canvas
            .add_event_listener_with_callback(end, handler.as_ref().unchecked_ref())
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
                let ratio = web_sys::window().map_or(1.0, |w| w.device_pixel_ratio()).max(1.0);
                let anchor = ScreenPoint::new(
                    event.client_x() as f64 * ratio,
                    event.client_y() as f64 * ratio,
                );
                // Exponential in the wheel delta, so zooming in and back out returns exactly
                // where it started. `vellum_app::input` argues this at length; the constant is
                // matched to it rather than invented.
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
            window
                .add_event_listener_with_callback("resize", handler.as_ref().unchecked_ref())
                .ok();
        }
        handler.forget();
    }
}
