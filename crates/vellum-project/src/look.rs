//! The measurements both front ends have to agree on.
//!
//! # Why this module exists
//!
//! Velm has two front ends over one engine: `vellum-app` drives it from a macOS window and
//! `vellum-web` from a browser canvas. Almost everything they need is already shared — the
//! document, the scene, the renderer, the text engine. What was *not* shared, and had to be,
//! is the handful of numbers that decide how an item **looks**: how far a sticky's words are
//! inset, how big a frame's name is, how far apart the grid's dots sit, how finely a pen
//! stroke is tessellated.
//!
//! Those were written twice, once in each front end, with identical values — and this
//! repository's standing rule is *one derivation, not two*, because it has paid for a second
//! copy three times already (`inspect.rs`'s `THEME_BORDER`, `mindmap.rs`'s hand-copied
//! colours, the icon `make-app.sh` draws in Python). The failure is never that the copies
//! disagree on the day they are written; it is that one of them is changed a year later.
//!
//! ⚠ **It happened here before the ink was dry.** The browser's grid dot was copied at the
//! `1.5` of feedback 11 — correct when the grid was a pale grey — while its *colour* was
//! copied from feedback 27, which made the token opaque black and reduced the dot to 1.0 in
//! the same round. Two values from two different moments in one file, drawing a 3×3 black
//! square where the desktop app draws a single pixel.
//!
//! # What belongs here, and what does not
//!
//! A number belongs here when **both** front ends need it and the board would look wrong if
//! they disagreed. A number stays in its front end when it is about that front end's own
//! surface — the desktop app's `FRAME_TITLE_MIN_DEVICE` floor stays in `draw.rs` because it
//! is about a caret and a cache the browser has neither of.

use vellum_render::UvRect;

// ---------------------------------------------------------------------------------------
// Stickies
// ---------------------------------------------------------------------------------------

/// Fraction of a sticky's box left as padding on each side.
///
/// Miro's own inset. Auto-fit measures against what is left, so this decides the type size as
/// well as the margin — and without it a sticky's words touch its edges, which is the single
/// thing that most makes a note look wrong. Paper has a margin.
pub const STICKY_PADDING: f64 = 0.08;

/// A sticky's box, less its padding: the area its words are actually set in.
pub fn sticky_text_box(width: f64, height: f64) -> (f64, f64) {
    (
        (width * (1.0 - STICKY_PADDING * 2.0)).max(1.0),
        (height * (1.0 - STICKY_PADDING * 2.0)).max(1.0),
    )
}

// ---------------------------------------------------------------------------------------
// Frames
// ---------------------------------------------------------------------------------------

/// A frame's name, as a fraction of the frame's height, and the range it is held to.
///
/// ⚠ **Never auto-fitted.** Auto-fit answers with the largest size that fits the box, and a
/// frame's box is the biggest thing on the board — so an auto-fitted title is enormous. The
/// size comes from the frame's own height, clamped, and the label is drawn *above* the top
/// edge rather than inside it, which is where Miro puts it.
pub const FRAME_TITLE_FRACTION: f64 = 0.03;
pub const FRAME_TITLE_MIN: f64 = 14.0;
pub const FRAME_TITLE_MAX: f64 = 96.0;

/// The size a frame of this height sets its name at, in world units.
pub fn frame_title_size(height: f64) -> f64 {
    (height * FRAME_TITLE_FRACTION).clamp(FRAME_TITLE_MIN, FRAME_TITLE_MAX)
}

// ---------------------------------------------------------------------------------------
// Link cards
// ---------------------------------------------------------------------------------------

/// Fraction of a card's width used as padding around its contents.
///
/// Measured off Miro's own card, from the reference the user sent: on a 422-unit-wide card
/// its picture is inset about 14 units a side. It is the one number that decides a card's
/// proportions, because the inset is taken from all four sides *and* between every row, so a
/// difference here compounds.
pub const CARD_PADDING: f64 = 0.035;

// ---------------------------------------------------------------------------------------
// The board's grid
// ---------------------------------------------------------------------------------------

/// The closest together the dots may sit, in device pixels, and the furthest apart.
///
/// The grid walks 1-2-5 × 10ⁿ and takes the first step whose on-screen spacing lands inside
/// this band, so the pair *selects* the step: moving one without the other widens the band
/// rather than shifting it, and the same step is chosen at most zooms.
pub const GRID_MIN_PIXELS: f64 = 14.0;
pub const GRID_MAX_PIXELS: f64 = 70.0;

/// Dot size at a nominal 900px-tall viewport, scaled with the viewport.
///
/// ⚠ **Tied to the grid's colour.** The token is pure opaque black, and that is only
/// legitimate because the dot is one device pixel; a 2×2 or 3×3 black square every 14–70
/// pixels is a texture rather than the whisper it is meant to be. Changing one of these two
/// without the other is exactly the mistake this module exists to make impossible.
pub const GRID_DOT: f32 = 1.0;

/// A hard ceiling on the grid, so a pathological zoom cannot emit a million quads.
///
/// ⚠ Checked **after** the line pattern is handled, never before. Lines cost `columns + rows`
/// quads where dots cost `columns × rows`, so gating both on one number makes graph paper
/// vanish on a display where only the dots were ever expensive.
pub const MAX_GRID_DOTS: i64 = 20_000;

/// The world-unit spacing whose on-screen size lands inside the readable band.
///
/// 1, 2, 5 × 10ⁿ, smallest first, so the step changes at the moment the previous one leaves
/// the band rather than at a round zoom. `None` means no step fits, which happens at a very
/// wide zoom and correctly draws nothing at all.
pub fn grid_step(zoom: f64) -> Option<f64> {
    if !zoom.is_finite() || zoom <= 0.0 {
        return None;
    }
    for decade in -3..=7 {
        for multiple in [1.0, 2.0, 5.0] {
            let step: f64 = multiple * 10f64.powi(decade);
            let on_screen = step * zoom;
            if (GRID_MIN_PIXELS..=GRID_MAX_PIXELS).contains(&on_screen) {
                return Some(step);
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------------------
// Ink
// ---------------------------------------------------------------------------------------

/// The zoom octave a stroke is tessellated for.
///
/// ⚠ **The caller passes `zoom × the item's own scale`, not the zoom.** A stroke is
/// tessellated in its own local space and then magnified on the GPU by `placement.scale`, so
/// what a viewer sees is the product — and deriving the tolerance from the zoom alone
/// tessellates an imported Miro stroke at scale 2 exactly twice too coarsely. Velm's own
/// strokes carry scale 1.0, which is why that survived a long time.
///
/// **`ceil`, not `round`.** Rounding to the nearest octave under-tessellates any zoom in the
/// upper half of a band by up to √2. Erring towards more triangles is the half of the error
/// nobody can see.
pub fn lod_band(zoom_times_scale: f64) -> i32 {
    if !zoom_times_scale.is_finite() || zoom_times_scale <= 0.0 {
        return 0;
    }
    zoom_times_scale.log2().ceil().clamp(-8.0, 8.0) as i32
}

// ---------------------------------------------------------------------------------------
// Pictures
// ---------------------------------------------------------------------------------------

/// The part of a texture that fills `into` without distorting it — `object-fit: cover`.
///
/// A card's image band has a fixed aspect and a page's `og:image` does not: a 1200×630 banner
/// is 1.90 wide and a product shot is 1.00, against a band near 1.47. Mapping the whole
/// texture onto the band scales each axis by a different amount, which is exactly what
/// "distorted" looks like, and the squarer the source the worse it is.
///
/// **Cover rather than contain**, deliberately. Both fix the distortion; they differ in what
/// they give up. `contain` fits the whole image and leaves empty bands, which on a slot sized
/// for a picture reads as a layout bug. `cover` fills it and trims the overflow **equally
/// from both sides**, so a centred subject stays centred — and these boards are product
/// photography, centred on white by convention. It is what Miro's own card does.
///
/// A degenerate box or texture answers `FULL`: a zero somewhere is a caller with nothing to
/// draw, and cropping to nothing would be worse than not cropping.
pub fn cover_uv(source: (u32, u32), into: (f64, f64)) -> UvRect {
    let (sw, sh) = (f64::from(source.0), f64::from(source.1));
    let (bw, bh) = into;
    if sw <= 0.0 || sh <= 0.0 || bw <= 0.0 || bh <= 0.0 {
        return UvRect::FULL;
    }
    // How much of each axis survives. Exactly one of these is 1.0 — the axis that already
    // matches — and the other is the ratio of the two aspects.
    let (source_aspect, box_aspect) = (sw / sh, bw / bh);
    let (keep_u, keep_v) = if source_aspect > box_aspect {
        (box_aspect / source_aspect, 1.0) // wider than the box: trim the sides
    } else {
        (1.0, source_aspect / box_aspect) // taller than the box: trim top and bottom
    };
    #[expect(clippy::cast_possible_truncation, reason = "a UV coordinate is 0..=1")]
    let uv = {
        let (u0, v0) = (((1.0 - keep_u) / 2.0) as f32, ((1.0 - keep_v) / 2.0) as f32);
        UvRect::new([u0, v0], [1.0 - u0, 1.0 - v0])
    };
    uv
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The assertion is about the **sampled aspect in texels**, not the UV numbers.
    ///
    /// `UvRect::FULL` happens to have the slot's aspect whenever the source already does,
    /// which is the case that used to pass by accident. Measuring what is sampled is what
    /// tells a working crop from a lucky one.
    #[test]
    fn a_picture_is_cropped_to_its_slot_rather_than_stretched_into_it() {
        for (source, into) in [
            ((1920u32, 1080u32), (300.0, 200.0)),
            ((600, 800), (300.0, 200.0)),
            ((1, 4000), (300.0, 200.0)),
        ] {
            let uv = cover_uv(source, into);
            let texels = (
                f64::from(uv.max[0] - uv.min[0]) * f64::from(source.0),
                f64::from(uv.max[1] - uv.min[1]) * f64::from(source.1),
            );
            let sampled = texels.0 / texels.1;
            let wanted = into.0 / into.1;
            assert!(
                (sampled / wanted - 1.0).abs() < 0.01,
                "{source:?} into {into:?} sampled {sampled:.3} against {wanted:.3}"
            );
        }
    }

    #[test]
    fn a_degenerate_box_or_texture_is_not_cropped() {
        assert_eq!(cover_uv((0, 100), (10.0, 10.0)), UvRect::FULL);
        assert_eq!(cover_uv((100, 100), (0.0, 10.0)), UvRect::FULL);
    }

    /// The grid's step is always inside the band that selects it, or there is no step.
    #[test]
    fn every_grid_step_lands_inside_the_band_that_chose_it() {
        for zoom in [0.01, 0.04, 0.25, 1.0, 4.0, 64.0] {
            if let Some(step) = grid_step(zoom) {
                let on_screen = step * zoom;
                assert!(
                    (GRID_MIN_PIXELS..=GRID_MAX_PIXELS).contains(&on_screen),
                    "at {zoom}x the step is {on_screen} device pixels"
                );
            }
        }
        assert_eq!(grid_step(0.0), None);
        assert_eq!(grid_step(f64::NAN), None);
    }

    /// ⚠ `ceil`, and the item's scale is the caller's job to fold in.
    #[test]
    fn a_stroke_is_tessellated_for_the_octave_above_its_drawn_scale() {
        assert_eq!(lod_band(1.0), 0);
        assert_eq!(lod_band(1.5), 1, "rounding down here is the under-tessellation bug");
        assert_eq!(lod_band(2.0), 1);
        assert_eq!(lod_band(0.5), -1);
        assert_eq!(lod_band(0.0), 0);
        assert_eq!(lod_band(f64::INFINITY), 0);
        // A stroke at scale 2 drawn at zoom 1 is seen at 2x and must be tessellated for it.
        assert_eq!(lod_band(1.0 * 2.0), lod_band(2.0));
    }

    #[test]
    fn a_frame_sets_its_name_from_its_own_height_within_bounds() {
        assert_eq!(frame_title_size(900.0), 27.0);
        assert_eq!(frame_title_size(10.0), FRAME_TITLE_MIN, "a tiny frame still has a name");
        assert_eq!(frame_title_size(100_000.0), FRAME_TITLE_MAX, "and a huge one is capped");
    }

    #[test]
    fn a_sticky_keeps_a_margin_on_every_side() {
        let (w, h) = sticky_text_box(100.0, 50.0);
        assert!((w - 84.0).abs() < 1e-9, "{w}");
        assert!((h - 42.0).abs() < 1e-9, "{h}");
        // A degenerate sticky still yields a usable box rather than a zero one.
        assert_eq!(sticky_text_box(0.0, 0.0), (1.0, 1.0));
    }
}
