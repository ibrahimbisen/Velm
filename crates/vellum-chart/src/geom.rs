//! The chart's coordinate space, and the shapes laid out in it.
//!
//! Charts are laid out in **the caller's own space**, not in a normalised box:
//! `build` takes the rectangle the chart widget occupies and every rect, point and
//! arc that comes back is already inside it. That differs from `vellum-shapes`,
//! where a shape is a description reused at any size, and it differs for a reason —
//! a chart is not scale-invariant. Tick labels, the 24px bar cap and the 2px gap
//! between stacked segments are all absolute sizes, so how many ticks fit and
//! whether a value label fits inside its bar can only be answered once, at the size
//! the chart is actually drawn.
//!
//! `f32` for geometry, `f64` for data. Geometry ends up in vertex buffers, where
//! `f32` is what the GPU takes; data values come from a user's table and get summed,
//! stacked and divided, where `f32`'s 7 digits would visibly lose cents on a total.
//! The boundary between the two is the scale in [`crate::scale`], and it is the only
//! place a `f64` becomes a `f32`.

use serde::{Deserialize, Serialize};

/// A point in chart space. y runs **downwards**, as it does on screen and in
/// `vellum-shapes`, so "the top of the plot" is the *smaller* y.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct Point {
    pub x: f32,
    pub y: f32,
}

/// Shorthand for [`Point::new`]; geometry reads better as `pt(4.0, 8.0)` than as a
/// struct literal, and this module is mostly point arithmetic.
pub const fn pt(x: f32, y: f32) -> Point {
    Point::new(x, y)
}

impl Point {
    pub const ZERO: Self = Self { x: 0.0, y: 0.0 };

    pub const fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }

    pub fn distance(self, other: Self) -> f32 {
        (self.x - other.x).hypot(self.y - other.y)
    }

    /// True when both components are finite. Every public constructor in this crate
    /// is expected to uphold this; the tests assert it over whole chart geometries,
    /// because one `NaN` reaching a vertex buffer silently deletes a triangle rather
    /// than failing loudly.
    pub fn is_finite(self) -> bool {
        self.x.is_finite() && self.y.is_finite()
    }
}

/// An axis-aligned rectangle given as origin plus size, because that is how every
/// consumer wants it: a bar is a rect, a label box is a rect, and both are built by
/// growing from a corner rather than by intersecting two extents.
///
/// `width` and `height` are non-negative by construction — [`Rect::from_edges`]
/// orders the edges it is given. A negative-size rect would hit-test as empty and
/// tessellate inside-out, which is a bug that only shows up on one dataset.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl Rect {
    pub const ZERO: Self = Self { x: 0.0, y: 0.0, width: 0.0, height: 0.0 };

    pub const fn new(x: f32, y: f32, width: f32, height: f32) -> Self {
        Self { x, y, width, height }
    }

    /// From two opposite edges in either order. Bars are built this way — the value
    /// end may be above or below the baseline depending on the sign of the datum,
    /// and the caller should not have to branch on it.
    pub fn from_edges(x0: f32, y0: f32, x1: f32, y1: f32) -> Self {
        Self {
            x: x0.min(x1),
            y: y0.min(y1),
            width: (x1 - x0).abs(),
            height: (y1 - y0).abs(),
        }
    }

    pub fn left(&self) -> f32 {
        self.x
    }

    pub fn right(&self) -> f32 {
        self.x + self.width
    }

    pub fn top(&self) -> f32 {
        self.y
    }

    pub fn bottom(&self) -> f32 {
        self.y + self.height
    }

    pub fn centre(&self) -> Point {
        Point::new(self.x + self.width * 0.5, self.y + self.height * 0.5)
    }

    pub fn centre_x(&self) -> f32 {
        self.x + self.width * 0.5
    }

    pub fn centre_y(&self) -> f32 {
        self.y + self.height * 0.5
    }

    pub fn is_empty(&self) -> bool {
        self.width <= 0.0 || self.height <= 0.0
    }

    /// Shrinks by `dx` on the left and right and `dy` on top and bottom, clamped so
    /// the result never inverts. Over-insetting is not an error — a plot area inset
    /// by more padding than it has room for is a legitimate consequence of a tiny
    /// widget, and it should collapse to nothing rather than turn inside out.
    pub fn inset(&self, dx: f32, dy: f32) -> Self {
        let width = (self.width - 2.0 * dx).max(0.0);
        let height = (self.height - 2.0 * dy).max(0.0);
        Self {
            x: self.x + (self.width - width) * 0.5,
            y: self.y + (self.height - height) * 0.5,
            width,
            height,
        }
    }

    /// Shrinks each edge independently, again clamped at collapse. This is how the
    /// axis bands and the legend band are carved out of the chart frame.
    ///
    /// When the insets exceed the rectangle the result collapses to nothing, and it
    /// collapses *inside*: a 20px chart still has an axis band wider than itself, and
    /// its plot area must end up at the edge of the frame rather than 24px to the
    /// right of it.
    pub fn inset_sides(&self, left: f32, top: f32, right: f32, bottom: f32) -> Self {
        let width = (self.width - left - right).max(0.0);
        let height = (self.height - top - bottom).max(0.0);
        Self {
            x: (self.x + left).min(self.right() - width),
            y: (self.y + top).min(self.bottom() - height),
            width,
            height,
        }
    }

    pub fn contains(&self, p: Point) -> bool {
        p.x >= self.left() && p.x <= self.right() && p.y >= self.top() && p.y <= self.bottom()
    }

    /// True when the two rectangles share interior area. Touching edges do **not**
    /// count: the label placer relies on that, since two labels separated by exactly
    /// the required gap are not in collision.
    pub fn intersects(&self, other: &Self) -> bool {
        self.left() < other.right()
            && other.left() < self.right()
            && self.top() < other.bottom()
            && other.top() < self.bottom()
    }

    /// True when `self` lies wholly inside `other`, with a tolerance for the
    /// accumulated rounding of a long chain of scale mappings.
    pub fn is_inside(&self, other: &Self) -> bool {
        const EPSILON: f32 = 1e-3;
        self.left() >= other.left() - EPSILON
            && self.right() <= other.right() + EPSILON
            && self.top() >= other.top() - EPSILON
            && self.bottom() <= other.bottom() + EPSILON
    }

    pub fn translated(&self, dx: f32, dy: f32) -> Self {
        Self { x: self.x + dx, y: self.y + dy, ..*self }
    }

    pub fn is_finite(&self) -> bool {
        self.x.is_finite() && self.y.is_finite() && self.width.is_finite() && self.height.is_finite()
    }
}

/// A straight run between two points: axis rules, gridlines, tick marks, and the
/// leader line from a nudged label back to the mark it belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Segment {
    pub from: Point,
    pub to: Point,
}

impl Segment {
    pub const fn new(from: Point, to: Point) -> Self {
        Self { from, to }
    }

    pub fn horizontal(y: f32, x0: f32, x1: f32) -> Self {
        Self::new(pt(x0, y), pt(x1, y))
    }

    pub fn vertical(x: f32, y0: f32, y1: f32) -> Self {
        Self::new(pt(x, y0), pt(x, y1))
    }

    pub fn is_finite(&self) -> bool {
        self.from.is_finite() && self.to.is_finite()
    }
}

/// A circular arc band — one pie or donut slice.
///
/// Kept as parameters rather than as a flattened polygon because the two consumers
/// want different things: an SDF or a shader-side ring wants the parameters, and a
/// `lyon` tessellation wants points. [`Arc::flatten`] produces the second from the
/// first at whatever tolerance the current zoom justifies, which is the same
/// level-of-detail argument `vellum-ink` makes for strokes.
///
/// Angles are in radians, measured **clockwise from twelve o'clock**, because that
/// is where every reader starts a pie. That is not the maths convention, and the
/// conversion lives in [`Arc::point_at`] so no caller has to remember it.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Arc {
    pub centre: Point,
    /// Zero for a pie, positive for a donut.
    pub inner_radius: f32,
    pub outer_radius: f32,
    pub start_angle: f32,
    /// Always `>= start_angle`; a slice sweeps forwards.
    pub end_angle: f32,
}

impl Arc {
    pub fn sweep(&self) -> f32 {
        self.end_angle - self.start_angle
    }

    pub fn mid_angle(&self) -> f32 {
        (self.start_angle + self.end_angle) * 0.5
    }

    /// The point at `angle` and `radius` on this arc's circle. Clockwise from
    /// twelve o'clock in a y-down space means `(sin, -cos)`.
    pub fn point_at(&self, angle: f32, radius: f32) -> Point {
        Point::new(
            self.centre.x + radius * angle.sin(),
            self.centre.y - radius * angle.cos(),
        )
    }

    /// The point where a label or leader line should leave the slice: on the mid
    /// angle, at `t` of the way out from the inner to the outer radius.
    pub fn anchor(&self, t: f32) -> Point {
        let radius = self.inner_radius + (self.outer_radius - self.inner_radius) * t;
        self.point_at(self.mid_angle(), radius)
    }

    /// The slice as a closed polygon, wound clockwise on the outer edge and back
    /// along the inner one.
    ///
    /// `tolerance` is the maximum sagitta — the distance the true arc is allowed to
    /// bulge away from the chord — in the same units as the radii, so passing half a
    /// screen pixel divided by the zoom gives a curve that is smooth at any zoom and
    /// cheap when zoomed out. The segment count comes from the standard
    /// `acos(1 - tol/r)` half-angle rather than from a fixed step, so a 4px donut in
    /// a legend costs a handful of points and a 400px one costs what it needs.
    pub fn flatten(&self, tolerance: f32) -> Vec<Point> {
        let sweep = self.sweep().abs();
        if self.outer_radius <= 0.0 || sweep <= 0.0 {
            return Vec::new();
        }
        let tolerance = tolerance.max(1e-4).min(self.outer_radius);
        // Half-angle whose chord stays within `tolerance` of the arc.
        let step = 2.0 * (1.0 - tolerance / self.outer_radius).clamp(-1.0, 1.0).acos();
        let segments = if step > 0.0 { (sweep / step).ceil() as usize } else { usize::MAX };
        let segments = segments.clamp(1, MAX_ARC_SEGMENTS);

        let mut points = Vec::with_capacity(segments * 2 + 2);
        for i in 0..=segments {
            let angle = self.start_angle + sweep * (i as f32 / segments as f32);
            points.push(self.point_at(angle, self.outer_radius));
        }
        if self.inner_radius > 0.0 {
            for i in (0..=segments).rev() {
                let angle = self.start_angle + sweep * (i as f32 / segments as f32);
                points.push(self.point_at(angle, self.inner_radius));
            }
        } else {
            points.push(self.centre);
        }
        points
    }

    pub fn is_finite(&self) -> bool {
        self.centre.is_finite()
            && self.inner_radius.is_finite()
            && self.outer_radius.is_finite()
            && self.start_angle.is_finite()
            && self.end_angle.is_finite()
    }
}

/// Ceiling on the points one slice may cost. A full circle at a 0.1px tolerance and
/// a 4000px radius asks for about 900 segments; anything past this is a degenerate
/// tolerance, not a smoother curve.
const MAX_ARC_SEGMENTS: usize = 1024;

/// A run of connected points — a line series, or the outline of an area fill.
///
/// Points, not curves: a data line must pass through its data and nothing between
/// two samples is known, so a spline through them would draw values the data does
/// not contain. `vellum-ink` smooths freehand strokes for exactly the opposite
/// reason — there the curve *is* the intent.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Polyline {
    pub points: Vec<Point>,
}

impl Polyline {
    pub fn new(points: Vec<Point>) -> Self {
        Self { points }
    }

    pub fn is_empty(&self) -> bool {
        self.points.is_empty()
    }

    pub fn len(&self) -> usize {
        self.points.len()
    }

    pub fn is_finite(&self) -> bool {
        self.points.iter().all(|p| p.is_finite())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rect_from_edges_orders_them_so_a_negative_bar_is_still_a_rect() {
        let up = Rect::from_edges(10.0, 100.0, 30.0, 40.0);
        let down = Rect::from_edges(10.0, 40.0, 30.0, 100.0);
        assert_eq!(up, down);
        assert_eq!(up, Rect::new(10.0, 40.0, 20.0, 60.0));
    }

    #[test]
    fn insetting_past_collapse_yields_an_empty_rect_not_an_inverted_one() {
        let r = Rect::new(0.0, 0.0, 10.0, 10.0).inset(20.0, 1.0);
        assert!(r.is_empty());
        assert_eq!(r.width, 0.0);
        assert!(r.height > 0.0);
    }

    #[test]
    fn a_collapsed_inset_stays_inside_the_rect_it_came_from() {
        let frame = Rect::new(0.0, 0.0, 20.0, 20.0);
        // An axis band wider than the whole chart.
        let plot = frame.inset_sides(40.0, 0.0, 0.0, 60.0);
        assert!(plot.is_empty());
        assert!(plot.is_inside(&frame), "{plot:?} escaped {frame:?}");
        // The ordinary case is untouched.
        assert_eq!(
            frame.inset_sides(4.0, 2.0, 1.0, 3.0),
            Rect::new(4.0, 2.0, 15.0, 15.0)
        );
    }

    #[test]
    fn touching_rects_do_not_intersect() {
        let a = Rect::new(0.0, 0.0, 10.0, 10.0);
        assert!(!a.intersects(&Rect::new(10.0, 0.0, 10.0, 10.0)));
        assert!(a.intersects(&Rect::new(9.9, 0.0, 10.0, 10.0)));
    }

    #[test]
    fn arc_angles_start_at_twelve_oclock_and_run_clockwise() {
        let arc = Arc {
            centre: Point::ZERO,
            inner_radius: 0.0,
            outer_radius: 10.0,
            start_angle: 0.0,
            end_angle: std::f32::consts::FRAC_PI_2,
        };
        let top = arc.point_at(0.0, 10.0);
        assert!(top.x.abs() < 1e-5 && (top.y + 10.0).abs() < 1e-5, "{top:?}");
        let right = arc.point_at(std::f32::consts::FRAC_PI_2, 10.0);
        assert!((right.x - 10.0).abs() < 1e-5 && right.y.abs() < 1e-5, "{right:?}");
    }

    #[test]
    fn flattening_respects_the_sagitta_tolerance() {
        let arc = Arc {
            centre: Point::ZERO,
            inner_radius: 0.0,
            outer_radius: 100.0,
            start_angle: 0.0,
            end_angle: std::f32::consts::TAU,
        };
        for tolerance in [0.05_f32, 0.5, 2.0] {
            let points = arc.flatten(tolerance);
            // Every chord midpoint must sit within `tolerance` of the true circle.
            for pair in points[..points.len() - 1].windows(2) {
                let mid = pt((pair[0].x + pair[1].x) * 0.5, (pair[0].y + pair[1].y) * 0.5);
                let sagitta = 100.0 - mid.distance(Point::ZERO);
                assert!(sagitta <= tolerance + 1e-3, "sagitta {sagitta} > tolerance {tolerance}");
            }
        }
        // Coarser tolerance must not cost more points than a fine one.
        assert!(arc.flatten(2.0).len() < arc.flatten(0.05).len());
    }

    #[test]
    fn a_donut_slice_flattens_to_a_closed_band_and_a_pie_slice_to_a_wedge() {
        let mut arc = Arc {
            centre: pt(50.0, 50.0),
            inner_radius: 20.0,
            outer_radius: 40.0,
            start_angle: 0.0,
            end_angle: 1.0,
        };
        let band = arc.flatten(0.25);
        assert!(band.iter().all(|p| p.is_finite()));
        // Half the ring is outer, half inner: distances come in two groups.
        assert!(band.iter().any(|p| (p.distance(arc.centre) - 20.0).abs() < 0.5));
        assert!(band.iter().any(|p| (p.distance(arc.centre) - 40.0).abs() < 0.5));

        arc.inner_radius = 0.0;
        let wedge = arc.flatten(0.25);
        assert_eq!(*wedge.last().unwrap(), arc.centre, "a pie slice closes on the centre");
    }

    #[test]
    fn degenerate_arcs_flatten_to_nothing_rather_than_panicking() {
        let zero = Arc {
            centre: Point::ZERO,
            inner_radius: 0.0,
            outer_radius: 0.0,
            start_angle: 0.0,
            end_angle: 1.0,
        };
        assert!(zero.flatten(0.5).is_empty());
        let empty_sweep = Arc { outer_radius: 10.0, end_angle: 0.0, ..zero };
        assert!(empty_sweep.flatten(0.5).is_empty());
        let silly_tolerance = Arc { outer_radius: 10.0, ..zero };
        assert!(!silly_tolerance.flatten(0.0).is_empty());
        assert!(silly_tolerance.flatten(0.0).len() <= MAX_ARC_SEGMENTS * 2 + 2);
    }
}
