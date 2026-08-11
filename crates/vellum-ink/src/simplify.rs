//! Level of detail: spending vertices only where the screen can show them.
//!
//! A 500-point stroke is 500 points because of how fast the hand moved, not because
//! the shape needs 500 points. Zoomed out to fit a board, most of those points land
//! inside the same screen pixel. The reference board carries **219 `paint` widgets**;
//! tessellating all of them at full density every frame is the difference between a
//! board that pans smoothly and one that does not.
//!
//! # Ramer-Douglas-Peucker
//!
//! RDP keeps the two endpoints, finds the point furthest from the line between them,
//! and recurses on both halves if that distance exceeds the tolerance. Its guarantee
//! is the useful one for rendering: **every discarded point lies within `tolerance`
//! of the polyline that remains**. Bounded worst-case error, not bounded average
//! error — the difference matters, because it is the single worst deviation that
//! shows up as a visible kink.
//!
//! Distance is measured to the line *segment*, not to the infinite line through the
//! endpoints. The infinite-line variant is common and cheaper, but its distances are
//! never larger, so it can discard a point that is genuinely far from the retained
//! polyline and quietly break the guarantee this module's callers rely on.
//!
//! The recursion is an explicit stack. RDP's depth is O(n) in the worst case — a
//! monotonically curving stroke splits off one point at a time — and a 5,000-point
//! stroke recursing natively is a stack overflow, which in a canvas app means losing
//! the user's board rather than dropping a frame.
//!
//! # Accuracy against vertex count
//!
//! Tolerance is a world-space distance, so screen accuracy at zoom `z` costs
//! `screen_tolerance / z` in world units: zoomed out to 10%, a quarter-pixel screen
//! budget permits 2.5 world px of error, and most of a stroke's points fall inside
//! it. Measured on a 512-point spiral, at the [`Lod::CRISP`] quarter-pixel budget:
//!
//! | zoom | world tolerance | points kept | of original |
//! |---|---|---|---|
//! | 4.0  | 0.0625 px | 456 | 89% |
//! | 1.0  | 0.25 px   | 237 | 46% |
//! | 0.25 | 1.0 px    | 120 | 23% |
//! | 0.1  | 2.5 px    | 79  | 15% |
//! | 0.01 | 25 px     | 24  | 5%  |
//!
//! Those numbers track `tolerance^(-1/2)` closely — every quadrupling of the
//! tolerance roughly halves the point count — which is the scaling a smooth curve
//! predicts and the reason the tradeoff is a good one. Accuracy is bought linearly
//! and paid for as a square root, so the curve stays believable long after the count
//! has collapsed.
//!
//! The tail is where the win is: the zoomed-out view that has to draw every stroke on
//! the board is exactly the one that needs almost none of their points.
//!
//! One caveat worth knowing before building a cache on top of this: RDP's output is
//! **not nested** across tolerances. Tightening the tolerance can retain a different
//! set of points rather than a superset of the previous one, so a level of detail
//! cannot be built by refining the one above it, and the stroke's shape shifts very
//! slightly as the LOD changes. Caching one mesh per zoom band rather than
//! re-simplifying per frame both avoids the cost and hides the shift.

use crate::stroke::{Stroke, StrokePoint};
use crate::vec2::Vec2;

/// How much geometric error the current view can hide.
///
/// Split into a screen-space budget and a zoom rather than expressed as one world
/// tolerance, because the budget is the part a human can reason about — "half a
/// pixel of error is invisible" is a statement about the screen, and it stays true
/// at every zoom while the world tolerance behind it moves by four orders of
/// magnitude across Vellum's 0.01–64 zoom range.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Lod {
    /// Camera zoom: world px multiplied by this gives screen px.
    pub zoom: f64,
    /// Permitted deviation in *screen* px.
    pub screen_tolerance: f64,
}

impl Lod {
    /// Sub-pixel. Indistinguishable from the unsimplified stroke; the setting for a
    /// static view or an export.
    pub const CRISP: f64 = 0.25;
    /// Half a pixel. The default: no visible difference on a smooth stroke, and
    /// roughly half the vertices of [`Lod::CRISP`].
    pub const BALANCED: f64 = 0.5;
    /// One pixel. Visible on a slow, deliberate curve; the setting for panning a
    /// board with hundreds of strokes on it, where a dropped frame is the more
    /// obvious artefact.
    pub const FAST: f64 = 1.0;

    /// The default budget at a given zoom.
    pub fn new(zoom: f64) -> Self {
        Self { zoom, screen_tolerance: Self::BALANCED }
    }

    pub fn with_screen_tolerance(zoom: f64, screen_tolerance: f64) -> Self {
        Self { zoom, screen_tolerance }
    }

    /// The budget converted to world px — the number every stage of the pipeline
    /// actually consumes.
    ///
    /// A non-positive or non-finite zoom falls back to 1:1 rather than producing an
    /// infinite tolerance, because an infinite tolerance simplifies every stroke on
    /// the board to a straight line and the cause is not obvious from the result.
    pub fn world_tolerance(&self) -> f64 {
        let zoom = if self.zoom.is_finite() && self.zoom > 0.0 { self.zoom } else { 1.0 };
        let screen = if self.screen_tolerance.is_finite() && self.screen_tolerance > 0.0 {
            self.screen_tolerance
        } else {
            Self::BALANCED
        };
        screen / zoom
    }
}

impl Default for Lod {
    fn default() -> Self {
        Self::new(1.0)
    }
}

/// Ramer-Douglas-Peucker. Returns the retained points, endpoints always included.
///
/// A non-positive or non-finite tolerance returns the input unchanged: "no error
/// budget" means "keep everything", which is the answer that cannot surprise anyone.
pub fn simplify(points: &[StrokePoint], tolerance: f64) -> Vec<StrokePoint> {
    // `tolerance.is_finite()` rather than a negated comparison, so that NaN is
    // rejected explicitly instead of by falling through a partial ordering.
    if points.len() <= 2 || !tolerance.is_finite() || tolerance <= 0.0 {
        return points.to_vec();
    }

    let last = points.len() - 1;
    let mut keep = vec![false; points.len()];
    keep[0] = true;
    keep[last] = true;

    let mut stack = vec![(0usize, last)];
    while let Some((a, b)) = stack.pop() {
        if b <= a + 1 {
            continue;
        }
        let (index, distance) = furthest_from_segment(points, a, b);
        if distance > tolerance {
            keep[index] = true;
            stack.push((a, index));
            stack.push((index, b));
        }
    }

    points.iter().zip(keep).filter(|(_, k)| *k).map(|(p, _)| *p).collect()
}

/// The interior point of `a..b` furthest from the segment `a→b`, and that distance.
fn furthest_from_segment(points: &[StrokePoint], a: usize, b: usize) -> (usize, f64) {
    let start = Vec2::from_point(points[a]);
    let end = Vec2::from_point(points[b]);
    let mut best = (a + 1, -1.0);
    for (i, p) in points.iter().enumerate().take(b).skip(a + 1) {
        let d = point_to_segment_distance(Vec2::from_point(*p), start, end);
        if d > best.1 {
            best = (i, d);
        }
    }
    best
}

/// Distance from `p` to the segment `a→b`, clamped at the endpoints.
///
/// Also correct when `a == b` — which happens on a closed stroke, where the first
/// RDP baseline has zero length and the infinite-line form would divide by zero.
pub(crate) fn point_to_segment_distance(p: Vec2, a: Vec2, b: Vec2) -> f64 {
    let ab = b - a;
    let length_squared = ab.length_squared();
    if length_squared <= 0.0 {
        return (p - a).length();
    }
    let t = ((p - a).dot(ab) / length_squared).clamp(0.0, 1.0);
    (p - (a + ab * t)).length()
}

impl Stroke {
    /// This stroke with points removed that no view at this tolerance could resolve.
    pub fn simplified(&self, tolerance: f64) -> Stroke {
        self.with_points(simplify(self.points(), tolerance))
    }

    /// [`Stroke::simplified`] with the tolerance the current view implies.
    pub fn simplified_for(&self, lod: Lod) -> Stroke {
        self.simplified(lod.world_tolerance())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn xy(points: &[(f64, f64)]) -> Stroke {
        Stroke::from_miro(points, Some(2.0))
    }

    /// A hand-drawn-ish spiral: continuously curving, unevenly sampled. The shape
    /// the LOD table in the module docs was measured on.
    fn spiral(n: usize) -> Stroke {
        let pts: Vec<_> = (0..n)
            .map(|i| {
                let t = i as f64 / n as f64;
                let angle = t * std::f64::consts::TAU * 3.0;
                let radius = 20.0 + 300.0 * t;
                (radius * angle.cos(), radius * angle.sin())
            })
            .collect();
        xy(&pts)
    }

    #[test]
    fn short_strokes_are_returned_unchanged() {
        for pts in [&[][..], &[(0.0, 0.0)][..], &[(0.0, 0.0), (5.0, 5.0)][..]] {
            let s = xy(pts);
            assert_eq!(s.simplified(10.0), s);
        }
    }

    #[test]
    fn endpoints_are_never_dropped() {
        let s = spiral(200);
        let out = s.simplified(1000.0);
        assert_eq!(out.len(), 2);
        assert_eq!(out.points()[0], s.points()[0]);
        assert_eq!(out.points()[1], *s.points().last().unwrap());
    }

    #[test]
    fn collinear_points_collapse_to_the_two_endpoints() {
        let s = xy(&[(0.0, 0.0), (10.0, 0.0), (20.0, 0.0), (30.0, 0.0), (40.0, 0.0)]);
        assert_eq!(s.simplified(0.001).len(), 2);
    }

    /// The guarantee the renderer depends on, checked directly rather than assumed
    /// from the algorithm's reputation.
    #[test]
    fn no_discarded_point_is_further_than_the_tolerance() {
        let s = spiral(512);
        for tolerance in [0.01, 0.25, 1.0, 5.0, 25.0] {
            let out = s.simplified(tolerance);
            for p in s.points() {
                let (distance, _, _) = crate::hit::distance_to_polyline((p.x, p.y), out.points());
                assert!(
                    distance <= tolerance + 1e-9,
                    "tolerance {tolerance}: point {p:?} is {distance} from the result"
                );
            }
        }
    }

    #[test]
    fn a_looser_tolerance_never_keeps_more_points() {
        let s = spiral(512);
        let mut previous = usize::MAX;
        for tolerance in [0.01, 0.06, 0.25, 1.0, 2.5, 25.0] {
            let n = s.simplified(tolerance).len();
            assert!(n <= previous, "tolerance {tolerance} kept {n} after {previous}");
            previous = n;
        }
    }

    /// A tolerance below the sampling error must be a no-op, or zooming in would
    /// keep changing the shape after it had already converged.
    #[test]
    fn a_tiny_tolerance_keeps_everything() {
        let s = spiral(64);
        assert_eq!(s.simplified(1e-12).len(), s.len());
    }

    #[test]
    fn a_non_positive_tolerance_keeps_everything() {
        let s = spiral(64);
        assert_eq!(s.simplified(0.0).len(), s.len());
        assert_eq!(s.simplified(-1.0).len(), s.len());
        assert_eq!(s.simplified(f64::NAN).len(), s.len());
    }

    /// A stroke that crosses itself must not be shortcut across the crossing: the
    /// two branches are far apart along the curve even though they meet in space.
    #[test]
    fn a_self_intersecting_stroke_keeps_both_branches() {
        let s = xy(&[
            (0.0, 0.0),
            (50.0, 50.0),
            (100.0, 0.0),
            (50.0, -50.0),
            (25.0, 25.0),
            (75.0, 25.0),
        ]);
        let out = s.simplified(1.0);
        assert_eq!(out.len(), s.len());
    }

    #[test]
    fn zoom_drives_the_world_tolerance() {
        assert_eq!(Lod::with_screen_tolerance(1.0, 0.25).world_tolerance(), 0.25);
        assert_eq!(Lod::with_screen_tolerance(0.1, 0.25).world_tolerance(), 2.5);
        assert_eq!(Lod::with_screen_tolerance(4.0, 0.25).world_tolerance(), 0.0625);
    }

    #[test]
    fn a_broken_zoom_falls_back_to_one_to_one() {
        assert_eq!(Lod::with_screen_tolerance(0.0, 0.5).world_tolerance(), 0.5);
        assert_eq!(Lod::with_screen_tolerance(-2.0, 0.5).world_tolerance(), 0.5);
        assert_eq!(Lod::with_screen_tolerance(f64::NAN, 0.5).world_tolerance(), 0.5);
        assert_eq!(Lod::with_screen_tolerance(1.0, f64::NAN).world_tolerance(), Lod::BALANCED);
    }

    /// The numbers quoted in the module docs. If simplification is ever retuned,
    /// this fails and the table gets updated rather than silently rotting.
    #[test]
    fn the_documented_lod_table_still_holds() {
        let s = spiral(512);
        for (zoom, expected) in [(4.0, 456), (1.0, 237), (0.25, 120), (0.1, 79), (0.01, 24)] {
            let lod = Lod::with_screen_tolerance(zoom, Lod::CRISP);
            let n = s.simplified_for(lod).len();
            assert_eq!(n, expected, "zoom {zoom}: documented {expected}, got {n}");
        }
    }

    #[test]
    fn distance_to_a_degenerate_segment_is_a_point_distance() {
        let a = crate::vec2::v(3.0, 4.0);
        assert_eq!(point_to_segment_distance(crate::vec2::v(0.0, 0.0), a, a), 5.0);
    }

    /// Segment distance, not infinite-line distance: past the end of the baseline
    /// these two disagree, and the segment answer is the one RDP's guarantee needs.
    #[test]
    fn distance_is_clamped_to_the_segment() {
        let (a, b) = (crate::vec2::v(0.0, 0.0), crate::vec2::v(10.0, 0.0));
        assert_eq!(point_to_segment_distance(crate::vec2::v(20.0, 0.0), a, b), 10.0);
        assert_eq!(point_to_segment_distance(crate::vec2::v(5.0, 3.0), a, b), 3.0);
    }
}
