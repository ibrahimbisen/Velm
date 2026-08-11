//! Level of detail for image residency: how large a texture has to be to survive
//! being drawn at the size it is *actually* drawn at.
//!
//! # Why this exists
//!
//! Eviction answers "what is the user not looking at". On the reference board that
//! question has no useful answer: at fit-zoom the whole board is on screen, so all
//! 122 images are in use at once and residency measured 785 MB against a 268 MB
//! budget — three times over, with nothing stale to throw away. `TextureManager`
//! logged exactly that and was right to.
//!
//! The board is not asking for 785 MB of *detail*, though. At 4 % zoom a 2048 px
//! photo occupies about 60 screen pixels. It is being sampled from a mip level five
//! steps down the chain, and the five levels above that one are paid for in full and
//! never read. Choosing the level from the drawn size is therefore not a quality
//! trade at all — the sampler was already going to use the small level. Keeping the
//! large ones was pure waste.
//!
//! # The policy, in four numbers
//!
//! Everything here is one decision per texture per frame, taken from the largest
//! whole-image texel count any of that frame's draws asked for:
//!
//! - **[`DetailPolicy::retain_slack_levels`]** — how much sharper than necessary the
//!   stored top level is kept. One level means a texture settles at twice the size it
//!   is drawn at, which absorbs a 2× zoom-in without touching the GPU and gives
//!   anisotropic filtering something to work with on a rotated image.
//! - **[`DetailPolicy::demote_threshold_levels`]** — how much surplus is worth acting
//!   on. Two levels (16× the memory) makes ordinary zooming free: only a real change
//!   of scale moves anything.
//! - **[`DetailPolicy::refinements_per_frame`]** — how many textures may be dropped
//!   per frame for being too coarse. Refinement costs a decode, and the caller's
//!   decode budget is per-frame, so asking for twenty at once would only produce
//!   twenty placeholders.
//! - **[`DetailPolicy::min_dimension`]** — a floor, so an image drawn at one pixel
//!   does not churn its way down to a 1 × 1 texture and back.
//!
//! The gap between the last two thresholds is the hysteresis. A texture demotes when
//! it has ≥ 3 levels of surplus and refines when it has < 0, so it settles two to
//! eight times the drawn size and stays there through any zoom smaller than 4×.
//!
//! # Why not BC7 or ASTC as well
//!
//! Considered and deliberately not done. Block compression is a constant 4–6× on the
//! resident set; picking the level by drawn size is ~50× on this board, and the two
//! do not compose — once a texture is 128 px because that is all the screen can show,
//! compressing it saves 60 KB. Against that it costs a CPU encoder (BC7 is minutes
//! per board without an SIMD crate, and the alternative is a new dependency on the
//! critical path of opening a file), a format that is not universally available —
//! `TEXTURE_COMPRESSION_BC` is an optional wgpu feature and Apple GPUs prefer ASTC —
//! and a quality floor on flat UI screenshots and text-heavy diagrams, which is most
//! of what this board's images are. Revisit if the resident set is ever dominated by
//! images that genuinely need their full resolution on screen at once.

use crate::texture::UvRect;

/// How the manager trades texture memory against sharpness.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DetailPolicy {
    /// Levels of headroom kept above what the drawn size needs.
    pub retain_slack_levels: u32,
    /// Surplus levels, past the slack, before a texture is rebuilt smaller.
    pub demote_threshold_levels: u32,
    /// Textures that may be dropped for re-upload at a finer level in one frame.
    pub refinements_per_frame: usize,
    /// A stored top level never goes below this on its longer side.
    pub min_dimension: u32,
}

impl Default for DetailPolicy {
    fn default() -> Self {
        Self {
            retain_slack_levels: 1,
            demote_threshold_levels: 2,
            refinements_per_frame: 4,
            min_dimension: 8,
        }
    }
}

/// What to do with one texture, given the size it was drawn at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Detail {
    Keep,
    /// Rebuild without this many levels off the top. Never more than the floor allows.
    Demote(u32),
    /// Too coarse for the size it is drawn at, and the source has more to give.
    Refine,
}

/// Nothing can demand more than this many texels of one image. A crop whose UV span
/// has collapsed to nothing would otherwise divide its way to infinity and take the
/// comparison with it.
const MAX_DEMAND: f32 = 65_536.0;

/// A UV span below this is treated as "no crop" rather than as a division. At 2048 px
/// — the largest texture stored — this is a sixteenth of a texel, so no real crop
/// reaches it.
const MIN_UV_SPAN: f32 = 1.0 / 32_768.0;

/// What one drawn instance asks of the **whole** image, in texels.
///
/// `size` is the instance's size in its view's units and `units_per_pixel` how many
/// of those units a screen pixel spans — so `size / units_per_pixel` is the drawn
/// size in device pixels, which for the board view is exactly `size × zoom`.
///
/// The crop is why this is not the answer on its own: an instance showing a tenth of
/// an image across 100 px needs a 1000 px image, not a 100 px one. Dividing by the UV
/// span converts "pixels on screen" into "texels of the whole thing", which is the
/// unit the stored size is in.
pub(crate) fn demanded_texels(size: [f32; 2], units_per_pixel: f32, crop: UvRect) -> (f32, f32) {
    let scale = if units_per_pixel.is_finite() && units_per_pixel > 0.0 {
        units_per_pixel
    } else {
        1.0
    };
    let axis = |extent: f32, span: f32| -> f32 {
        if !extent.is_finite() {
            return MAX_DEMAND;
        }
        let pixels = extent.abs() / scale;
        let span = span.abs();
        let texels = if span > MIN_UV_SPAN { pixels / span } else { pixels };
        texels.clamp(0.0, MAX_DEMAND)
    };
    (
        axis(size[0], crop.max[0] - crop.min[0]),
        axis(size[1], crop.max[1] - crop.min[1]),
    )
}

/// How many mip levels of `stored` are surplus to `demanded`.
///
/// Positive means the texture is that many halvings sharper than anything asked for;
/// negative means it is being magnified and is that many halvings too coarse. The
/// tighter of the two axes wins, because losing detail on one axis is visible whatever
/// the other one is doing.
pub(crate) fn surplus_levels(stored: (u32, u32), demanded: (f32, f32)) -> i32 {
    let ratio_w = stored.0.max(1) as f32 / demanded.0.max(1.0);
    let ratio_h = stored.1.max(1) as f32 / demanded.1.max(1.0);
    let ratio = ratio_w.min(ratio_h);
    if !ratio.is_finite() || ratio <= 0.0 {
        return 0;
    }
    ratio.log2().floor().clamp(-32.0, 32.0) as i32
}

/// The decision for one texture. `ceiling` is the finest top level the source could
/// be re-uploaded at, so an image already at its source resolution is never asked to
/// refine — it is simply being magnified, which is the document's choice and not a
/// residency problem.
pub(crate) fn decide(
    policy: &DetailPolicy,
    stored: (u32, u32),
    ceiling: (u32, u32),
    demanded: (f32, f32),
) -> Detail {
    // Saturating rather than `as i32`: a caller that disables demotion with
    // `u32::MAX` would otherwise wrap to -1 and demote everything.
    let levels = |value: u32| -> i32 { value.min(i32::MAX as u32) as i32 };

    let surplus = surplus_levels(stored, demanded);
    let droppable = surplus - levels(policy.retain_slack_levels);
    if droppable >= levels(policy.demote_threshold_levels.max(1)) {
        let allowed = max_drop(stored, policy.min_dimension) as i32;
        let drop = droppable.min(allowed);
        return if drop > 0 { Detail::Demote(drop as u32) } else { Detail::Keep };
    }
    if surplus < 0 && (stored.0 < ceiling.0 || stored.1 < ceiling.1) {
        return Detail::Refine;
    }
    Detail::Keep
}

/// Levels that can come off before the longer side falls under the floor.
fn max_drop(stored: (u32, u32), min_dimension: u32) -> u32 {
    let floor = min_dimension.max(1);
    let longest = stored.0.max(stored.1);
    let mut levels = 0;
    while levels < 31 && (longest >> (levels + 1)) >= floor {
        levels += 1;
    }
    levels
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> DetailPolicy {
        DetailPolicy::default()
    }

    /// The board view's `units_per_pixel` is `1/zoom`, so this is the whole reason the
    /// fix works: a 4000 px-wide item at 4 % is sixty pixels of screen.
    #[test]
    fn the_demand_is_the_drawn_size_in_device_pixels() {
        let (w, h) = demanded_texels([1500.0, 1000.0], 1.0 / 0.04, UvRect::FULL);
        assert_eq!((w.round(), h.round()), (60.0, 40.0));

        let (w, h) = demanded_texels([1500.0, 1000.0], 1.0, UvRect::FULL);
        assert_eq!((w, h), (1500.0, 1000.0));
    }

    /// A crop showing a quarter of an image across 100 px needs a 400 px image. Miss
    /// this and every cropped photo on the board is stored four times too small.
    #[test]
    fn a_crop_scales_the_demand_up_by_its_span() {
        let quarter = UvRect::new([0.25, 0.25], [0.5, 0.5]);
        let (w, h) = demanded_texels([100.0, 100.0], 1.0, quarter);
        assert_eq!((w, h), (400.0, 400.0));
    }

    /// A collapsed crop, a NaN size out of a degenerate camera: the demand has to stay
    /// finite, because it is about to be compared against a stored size.
    #[test]
    fn a_degenerate_instance_produces_a_finite_demand() {
        let collapsed = UvRect::new([0.5, 0.5], [0.5, 0.5]);
        let (w, h) = demanded_texels([100.0, 100.0], 1.0, collapsed);
        assert_eq!((w, h), (100.0, 100.0), "a zero span is no crop, not a division");

        for bad in [0.0, -1.0, f32::NAN, f32::INFINITY] {
            let (w, h) = demanded_texels([100.0, 100.0], bad, UvRect::FULL);
            assert!(w.is_finite() && h.is_finite(), "units_per_pixel {bad}");
        }
        let (w, h) = demanded_texels([f32::NAN, f32::INFINITY], 1.0, UvRect::FULL);
        assert!(w.is_finite() && h.is_finite());
    }

    #[test]
    fn surplus_counts_halvings_and_takes_the_tighter_axis() {
        assert_eq!(surplus_levels((2048, 2048), (64.0, 64.0)), 5);
        assert_eq!(surplus_levels((2048, 2048), (2048.0, 2048.0)), 0);
        assert_eq!(surplus_levels((512, 512), (1024.0, 1024.0)), -1);
        // Sharp on one axis, coarse on the other: the coarse one decides.
        assert_eq!(surplus_levels((2048, 64), (64.0, 64.0)), 0);
        assert_eq!(surplus_levels((1, 1), (0.0, 0.0)), 0);
    }

    /// The measured case. A 2048 px photo drawn at 60 px settles at 128 — two levels
    /// above what the sampler reads — and then stops moving.
    #[test]
    fn a_photo_at_fit_zoom_settles_two_levels_above_the_drawn_size() {
        let ceiling = (2048, 2048);
        assert_eq!(
            decide(&policy(), (2048, 2048), ceiling, (60.0, 40.0)),
            Detail::Demote(4)
        );
        assert_eq!(decide(&policy(), (128, 128), ceiling, (60.0, 40.0)), Detail::Keep);
    }

    /// Hysteresis is the whole point of the two thresholds: a zoom that changes the
    /// drawn size by less than 4× must not move a single texture, or panning across a
    /// board of photos would rebuild them continuously.
    #[test]
    fn zooming_within_four_times_moves_nothing() {
        let ceiling = (2048, 2048);
        for demanded in [128.0, 200.0, 256.0, 400.0, 512.0] {
            assert_eq!(
                decide(&policy(), (512, 512), ceiling, (demanded, demanded)),
                Detail::Keep,
                "demanded {demanded} against a 512 px texture"
            );
        }
    }

    #[test]
    fn a_texture_too_coarse_for_its_draw_asks_for_a_finer_one() {
        assert_eq!(
            decide(&policy(), (128, 128), (2048, 2048), (1600.0, 1600.0)),
            Detail::Refine
        );
    }

    /// An image magnified past its own source resolution is the document's choice.
    /// Asking to refine it would drop and re-upload the same pixels every frame.
    #[test]
    fn an_image_already_at_its_source_resolution_never_refines() {
        assert_eq!(
            decide(&policy(), (256, 256), (256, 256), (4000.0, 4000.0)),
            Detail::Keep
        );
    }

    /// Demote and refine must not both be reachable from one stored size, or a texture
    /// could oscillate. Checked across the whole range of demands.
    #[test]
    fn no_demand_both_demotes_and_refines_the_result() {
        let ceiling = (2048, 2048);
        let mut demanded = 0.5f32;
        while demanded < 8192.0 {
            let stored = match decide(&policy(), (2048, 2048), ceiling, (demanded, demanded)) {
                Detail::Demote(k) => (2048 >> k, 2048 >> k),
                _ => (2048, 2048),
            };
            assert_ne!(
                decide(&policy(), stored, ceiling, (demanded, demanded)),
                Detail::Refine,
                "demand {demanded} demoted to {stored:?} and immediately wanted refining"
            );
            demanded *= 1.05;
        }
    }

    #[test]
    fn the_floor_stops_a_one_pixel_draw_reaching_a_one_texel_texture() {
        assert_eq!(max_drop((2048, 2048), 8), 8, "2048 -> 8");
        assert_eq!(max_drop((8, 8), 8), 0);
        assert_eq!(max_drop((4, 4), 8), 0, "already under the floor");
        // The longer side is what is measured, so a 2048 × 16 strip may still come
        // down to 8 × 1 — a strip that thin has no detail on its short axis to lose.
        assert_eq!(max_drop((2048, 16), 8), 8);

        let tiny = decide(&policy(), (2048, 2048), (2048, 2048), (1.0, 1.0));
        assert_eq!(tiny, Detail::Demote(8));
    }

    /// A caller can turn the whole thing off, and must get the old behaviour exactly.
    #[test]
    fn a_disabled_policy_keeps_everything() {
        let off = DetailPolicy {
            demote_threshold_levels: u32::MAX,
            refinements_per_frame: 0,
            ..DetailPolicy::default()
        };
        assert_eq!(decide(&off, (2048, 2048), (2048, 2048), (1.0, 1.0)), Detail::Keep);
    }
}
