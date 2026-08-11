//! The geometry vocabulary connectors are routed in: points, directions, rectangles
//! and polylines, all in `f64` world space.
//!
//! These types are deliberately local rather than borrowed from `vellum-scene`.
//! Routing needs vector arithmetic — rotations, normals, projections — that a
//! spatial index has no reason to carry, and pointing a pure-geometry crate at the
//! R-tree crate would invert the dependency direction in `docs/01-architecture.md`
//! for the sake of two field names. The conversion at the boundary is a struct
//! literal.
//!
//! **`+x` is right, `+y` is down**, matching Miro and the rest of Vellum. That sign
//! convention is why [`Vec2::left_normal`] returns `(y, -x)` and why a *positive*
//! rotation angle turns clockwise on screen: with `y` pointing down, the textbook
//! anticlockwise rotation matrix reads as clockwise. Getting this backwards rotates
//! every connector anchor the wrong way around its widget, which looks like a
//! plausible layout rather than a bug.

use serde::{Deserialize, Serialize};

/// Lengths below this are treated as zero.
///
/// World space is `f64` px over boards tens of thousands of px wide, so `1e-9` is
/// far below any distance a user can express and far above the accumulated error of
/// the handful of multiply-adds a route performs.
pub const EPSILON: f64 = 1e-9;

/// A point on the board, in world pixels.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

impl Point {
    pub const ORIGIN: Self = Self { x: 0.0, y: 0.0 };

    pub const fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }

    pub fn distance_to(self, other: Self) -> f64 {
        (other - self).length()
    }

    /// Linear interpolation. `t = 0` is `self`, `t = 1` is `other`; values outside
    /// `0..=1` extrapolate, which is what segment trimming relies on.
    pub fn lerp(self, other: Self, t: f64) -> Self {
        Self::new(self.x + (other.x - self.x) * t, self.y + (other.y - self.y) * t)
    }

    pub fn midpoint(self, other: Self) -> Self {
        self.lerp(other, 0.5)
    }

    pub fn is_finite(self) -> bool {
        self.x.is_finite() && self.y.is_finite()
    }

    /// True when two points are within [`EPSILON`] of each other. Used instead of
    /// `==` everywhere a coincidence decides control flow, because a coincident pair
    /// that compares unequal produces a zero-length direction vector and, from
    /// there, a `NaN` route.
    pub fn coincides_with(self, other: Self) -> bool {
        self.distance_to(other) <= EPSILON
    }
}

impl std::ops::Sub for Point {
    type Output = Vec2;

    fn sub(self, rhs: Self) -> Vec2 {
        Vec2::new(self.x - rhs.x, self.y - rhs.y)
    }
}

impl std::ops::Add<Vec2> for Point {
    type Output = Self;

    fn add(self, rhs: Vec2) -> Self {
        Self::new(self.x + rhs.x, self.y + rhs.y)
    }
}

impl std::ops::Sub<Vec2> for Point {
    type Output = Self;

    fn sub(self, rhs: Vec2) -> Self {
        Self::new(self.x - rhs.x, self.y - rhs.y)
    }
}

/// A direction or displacement in world space. Distinct from [`Point`] so that
/// "where something is" and "which way it faces" cannot be swapped by accident —
/// the two are structurally identical and the mistake is invisible in a screenshot.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct Vec2 {
    pub x: f64,
    pub y: f64,
}

impl Vec2 {
    pub const ZERO: Self = Self { x: 0.0, y: 0.0 };
    /// Unit vector pointing right.
    pub const X: Self = Self { x: 1.0, y: 0.0 };
    /// Unit vector pointing *down* — `+y` is down in world space.
    pub const Y: Self = Self { x: 0.0, y: 1.0 };

    pub const fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }

    pub fn length(self) -> f64 {
        self.length_squared().sqrt()
    }

    pub fn length_squared(self) -> f64 {
        self.x * self.x + self.y * self.y
    }

    /// Scales to unit length, or `None` if the vector is too short to have a
    /// meaningful direction. Returning `Option` rather than a zero vector forces
    /// every caller to decide what a directionless endpoint means; silently
    /// returning `(0, 0)` propagates into control points and produces a route that
    /// collapses to a dot.
    pub fn normalized(self) -> Option<Self> {
        let len = self.length();
        if len <= EPSILON { None } else { Some(Self::new(self.x / len, self.y / len)) }
    }

    pub fn scaled(self, factor: f64) -> Self {
        Self::new(self.x * factor, self.y * factor)
    }

    pub fn dot(self, other: Self) -> f64 {
        self.x * other.x + self.y * other.y
    }

    /// The normal 90° to the *left* of this vector as seen on screen. With `+y`
    /// down, that is `(y, -x)`: pointing right `(1, 0)` yields `(0, -1)`, which is
    /// up. Jump-over arcs bulge along this direction so that every hop on a board
    /// leans the same way.
    pub fn left_normal(self) -> Self {
        Self::new(self.y, -self.x)
    }

    /// Rotates clockwise on screen by `degrees`. See the module docs for why the
    /// textbook anticlockwise matrix is a clockwise rotation here.
    pub fn rotated_degrees(self, degrees: f64) -> Self {
        if degrees == 0.0 {
            return self;
        }
        let (sin, cos) = degrees.to_radians().sin_cos();
        Self::new(self.x * cos - self.y * sin, self.x * sin + self.y * cos)
    }

    /// True when the vector lies along an axis, within [`EPSILON`] of it. The
    /// orthogonal router uses this to decide whether an endpoint imposes a
    /// departure axis at all — a connector leaving a rotated widget generally does
    /// not.
    pub fn is_axis_aligned(self) -> bool {
        self.x.abs() <= EPSILON || self.y.abs() <= EPSILON
    }
}

impl std::ops::Neg for Vec2 {
    type Output = Self;

    fn neg(self) -> Self {
        Self::new(-self.x, -self.y)
    }
}

impl std::ops::Add for Vec2 {
    type Output = Self;

    fn add(self, rhs: Self) -> Self {
        Self::new(self.x + rhs.x, self.y + rhs.y)
    }
}

impl std::ops::Sub for Vec2 {
    type Output = Self;

    fn sub(self, rhs: Self) -> Self {
        Self::new(self.x - rhs.x, self.y - rhs.y)
    }
}

/// An axis-aligned rectangle, stored as its two extreme corners and normalised so
/// `min <= max` on both axes.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Rect {
    pub min: Point,
    pub max: Point,
}

impl Rect {
    pub fn from_corners(a: Point, b: Point) -> Self {
        Self {
            min: Point::new(a.x.min(b.x), a.y.min(b.y)),
            max: Point::new(a.x.max(b.x), a.y.max(b.y)),
        }
    }

    pub fn from_center_size(center: Point, width: f64, height: f64) -> Self {
        let half = Vec2::new(width.abs() / 2.0, height.abs() / 2.0);
        Self { min: center - half, max: center + half }
    }

    /// The tightest rectangle containing every point, or `None` for an empty
    /// iterator.
    pub fn from_points(points: impl IntoIterator<Item = Point>) -> Option<Self> {
        let mut iter = points.into_iter();
        let first = iter.next()?;
        let mut rect = Self::from_corners(first, first);
        for p in iter {
            rect = rect.union_point(p);
        }
        Some(rect)
    }

    pub fn width(self) -> f64 {
        self.max.x - self.min.x
    }

    pub fn height(self) -> f64 {
        self.max.y - self.min.y
    }

    pub fn center(self) -> Point {
        self.min.midpoint(self.max)
    }

    /// Edge-inclusive containment.
    pub fn contains(self, p: Point) -> bool {
        p.x >= self.min.x && p.x <= self.max.x && p.y >= self.min.y && p.y <= self.max.y
    }

    /// Edge-inclusive overlap.
    pub fn intersects(self, other: Self) -> bool {
        self.min.x <= other.max.x
            && self.max.x >= other.min.x
            && self.min.y <= other.max.y
            && self.max.y >= other.min.y
    }

    pub fn inflate(self, amount: f64) -> Self {
        Self {
            min: Point::new(self.min.x - amount, self.min.y - amount),
            max: Point::new(self.max.x + amount, self.max.y + amount),
        }
    }

    pub fn union(self, other: Self) -> Self {
        Self {
            min: Point::new(self.min.x.min(other.min.x), self.min.y.min(other.min.y)),
            max: Point::new(self.max.x.max(other.max.x), self.max.y.max(other.max.y)),
        }
    }

    pub fn union_point(self, p: Point) -> Self {
        self.union(Self::from_corners(p, p))
    }

    /// Whether the segment `a`–`b` passes through the rectangle's **interior**.
    ///
    /// Interior, not closure, and the distinction is the whole point: the
    /// orthogonal router walks lattice lines that sit exactly on obstacles'
    /// inflated edges, so a closed test would reject every route that hugs an
    /// obstacle at precisely the requested clearance — which is the route it is
    /// supposed to prefer.
    pub fn intersects_segment(self, a: Point, b: Point) -> bool {
        let d = b - a;
        let (mut enter, mut exit) = (0.0_f64, 1.0_f64);

        for (origin, delta, lo, hi) in
            [(a.x, d.x, self.min.x, self.max.x), (a.y, d.y, self.min.y, self.max.y)]
        {
            if delta.abs() <= EPSILON {
                // Parallel to this slab: the segment can only reach the interior if
                // it already lies strictly between the slab's planes.
                if origin <= lo + EPSILON || origin >= hi - EPSILON {
                    return false;
                }
            } else {
                let (t0, t1) = ((lo - origin) / delta, (hi - origin) / delta);
                enter = enter.max(t0.min(t1));
                exit = exit.min(t0.max(t1));
                if enter >= exit {
                    return false;
                }
            }
        }

        // A segment grazing a corner clips to a zero-length overlap; that is contact,
        // not penetration.
        (exit - enter) * d.length() > EPSILON
    }

    /// How far along `direction` a ray from `origin` travels before it is clear of
    /// the rectangle. `0` when the ray never enters or is already past it.
    ///
    /// Used to push a connector's departure stub outside its own widget: an anchor
    /// sits *on* the widget's edge, so a fixed stub length is not enough to clear
    /// the inflated obstacle the same widget contributes.
    pub fn ray_exit_distance(self, origin: Point, direction: Vec2) -> f64 {
        let mut exit = f64::INFINITY;
        for (o, d, lo, hi) in [
            (origin.x, direction.x, self.min.x, self.max.x),
            (origin.y, direction.y, self.min.y, self.max.y),
        ] {
            if d.abs() <= EPSILON {
                if o < lo || o > hi {
                    return 0.0;
                }
            } else {
                exit = exit.min(if d > 0.0 { (hi - o) / d } else { (lo - o) / d });
            }
        }
        if exit.is_finite() { exit.max(0.0) } else { 0.0 }
    }
}

/// A flattened path: the form routing results take for measurement, dashing,
/// hit-testing and tessellation.
///
/// Curves are flattened once, up front, rather than handed to lyon as curves. That
/// costs a negligible amount of accuracy at the tolerances involved and buys one
/// important property: dashed and solid connectors, and hit-testing and rendering,
/// all measure arc length along *the same* polyline. A dash pattern computed
/// against the analytic curve but drawn against lyon's flattening would drift.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Polyline {
    pub points: Vec<Point>,
}

impl Polyline {
    /// Builds a polyline, dropping consecutive duplicates. Duplicates would
    /// contribute zero-length segments, which yield no direction and therefore no
    /// stroke geometry, and would make a zero-length connector look like a
    /// two-point path that it is not.
    pub fn new(points: impl IntoIterator<Item = Point>) -> Self {
        let mut out: Vec<Point> = Vec::new();
        for p in points {
            if out.last().is_none_or(|last| !last.coincides_with(p)) {
                out.push(p);
            }
        }
        Self { points: out }
    }

    pub fn is_empty(&self) -> bool {
        self.points.is_empty()
    }

    pub fn len(&self) -> usize {
        self.points.len()
    }

    pub fn length(&self) -> f64 {
        self.points.windows(2).map(|w| w[0].distance_to(w[1])).sum()
    }

    pub fn bounds(&self) -> Option<Rect> {
        Rect::from_points(self.points.iter().copied())
    }

    /// Shortest distance from `p` to the polyline. `INFINITY` for an empty
    /// polyline, and the plain point distance for a single-point one — which is the
    /// zero-length-connector case, where hit-testing should still find the dot the
    /// user placed.
    pub fn distance_to(&self, p: Point) -> f64 {
        match self.points.as_slice() {
            [] => f64::INFINITY,
            [only] => only.distance_to(p),
            points => points
                .windows(2)
                .map(|w| distance_to_segment(p, w[0], w[1]))
                .fold(f64::INFINITY, f64::min),
        }
    }

    /// Removes `from_start` and `from_end` of arc length from the two ends.
    ///
    /// Used to stop a line short of a filled arrowhead. Miro does exactly this: on
    /// the reference board a 368.15px connector is drawn as `M 0 0 L 357.97 0`,
    /// stopping 10.18px short of the tip its arrowhead occupies.
    pub fn trimmed(&self, from_start: f64, from_end: f64) -> Self {
        let total = self.length();
        let (start, end) = (from_start.max(0.0), from_end.max(0.0));
        if start + end >= total - EPSILON {
            // Trimming would consume the whole line. Keep nothing rather than
            // emitting an inverted stub.
            return Self::default();
        }
        self.sub_polyline(start, total - end)
    }

    /// The portion of the polyline between two arc-length positions.
    pub fn sub_polyline(&self, from: f64, to: f64) -> Self {
        if self.points.len() < 2 || to <= from + EPSILON {
            return Self::default();
        }
        let mut out = Vec::new();
        let mut travelled = 0.0;
        for w in self.points.windows(2) {
            let (a, b) = (w[0], w[1]);
            let seg_len = a.distance_to(b);
            if seg_len <= EPSILON {
                continue;
            }
            let (seg_start, seg_end) = (travelled, travelled + seg_len);
            travelled = seg_end;
            if seg_end <= from || seg_start >= to {
                continue;
            }
            let t0 = ((from - seg_start) / seg_len).clamp(0.0, 1.0);
            let t1 = ((to - seg_start) / seg_len).clamp(0.0, 1.0);
            if out.is_empty() {
                out.push(a.lerp(b, t0));
            }
            out.push(a.lerp(b, t1));
        }
        Self::new(out)
    }

    /// The point at an arc-length position, clamped to the ends.
    pub fn point_at(&self, distance: f64) -> Option<Point> {
        let first = *self.points.first()?;
        if distance <= 0.0 {
            return Some(first);
        }
        let mut travelled = 0.0;
        for w in self.points.windows(2) {
            let seg_len = w[0].distance_to(w[1]);
            if travelled + seg_len >= distance {
                return Some(w[0].lerp(w[1], (distance - travelled) / seg_len));
            }
            travelled += seg_len;
        }
        self.points.last().copied()
    }
}

/// Shortest distance from a point to a line segment.
pub fn distance_to_segment(p: Point, a: Point, b: Point) -> f64 {
    let ab = b - a;
    let len_sq = ab.length_squared();
    if len_sq <= EPSILON {
        return p.distance_to(a);
    }
    let t = ((p - a).dot(ab) / len_sq).clamp(0.0, 1.0);
    p.distance_to(a + ab.scaled(t))
}

/// The single point where two segments **properly** cross — each passing through
/// the other's interior — or `None`.
///
/// Two exclusions, both deliberate, and both there because the caller is jump-over
/// insertion:
///
/// - **Collinear overlap.** Two connectors running along one another do not have
///   *a* crossing point, and a hop placed at an arbitrary point of the overlap looks
///   like a defect rather than a crossing.
/// - **Endpoint contact.** Connectors meeting at a shared widget touch at their
///   anchors, and hopping there would put an arc on top of the widget they are both
///   attached to. A T-junction, where one connector ends on another's flank, is the
///   same case.
///
/// The endpoint exclusion also swallows the measure-zero case of a crossing landing
/// exactly on a polyline vertex, which is then missed rather than hopped. A hop
/// straddling a corner would be worse.
pub fn segment_intersection(a0: Point, a1: Point, b0: Point, b1: Point) -> Option<Point> {
    let r = a1 - a0;
    let s = b1 - b0;
    let denom = r.x * s.y - r.y * s.x;
    if denom.abs() <= EPSILON {
        return None;
    }
    let q = b0 - a0;
    let t = (q.x * s.y - q.y * s.x) / denom;
    let u = (q.x * r.y - q.y * r.x) / denom;
    if !(0.0..=1.0).contains(&t) || !(0.0..=1.0).contains(&u) {
        return None;
    }
    let at = a0 + r.scaled(t);
    if [a0, a1, b0, b1].iter().any(|end| end.coincides_with(at)) {
        return None;
    }
    Some(at)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn a_positive_rotation_turns_clockwise_on_screen() {
        // Right (1, 0) turned 90° must point down (0, 1), because +y is down.
        let r = Vec2::X.rotated_degrees(90.0);
        assert!(close(r.x, 0.0) && close(r.y, 1.0), "{r:?}");
    }

    #[test]
    fn left_normal_of_rightward_points_up() {
        let n = Vec2::X.left_normal();
        assert_eq!(n, Vec2::new(0.0, -1.0));
    }

    #[test]
    fn zero_length_vectors_have_no_direction() {
        assert_eq!(Vec2::ZERO.normalized(), None);
        assert_eq!(Vec2::new(1e-12, 0.0).normalized(), None);
        assert_eq!(Vec2::new(3.0, 4.0).normalized(), Some(Vec2::new(0.6, 0.8)));
    }

    #[test]
    fn segment_touching_an_edge_does_not_penetrate() {
        let r = Rect::from_corners(Point::new(0.0, 0.0), Point::new(10.0, 10.0));
        // Exactly along the top edge: allowed, this is the clearance line.
        assert!(!r.intersects_segment(Point::new(-5.0, 0.0), Point::new(15.0, 0.0)));
        // One unit inside: blocked.
        assert!(r.intersects_segment(Point::new(-5.0, 1.0), Point::new(15.0, 1.0)));
        // Grazing a corner is contact, not penetration.
        assert!(!r.intersects_segment(Point::new(-5.0, 5.0), Point::new(0.0, 0.0)));
    }

    #[test]
    fn segment_entirely_outside_is_clear() {
        let r = Rect::from_corners(Point::new(0.0, 0.0), Point::new(10.0, 10.0));
        assert!(!r.intersects_segment(Point::new(-5.0, -5.0), Point::new(-1.0, 20.0)));
        assert!(!r.intersects_segment(Point::new(20.0, 5.0), Point::new(30.0, 5.0)));
    }

    #[test]
    fn ray_exit_clears_the_rectangle_from_a_point_on_its_edge() {
        let r = Rect::from_corners(Point::new(0.0, 0.0), Point::new(10.0, 10.0));
        // Starting on the right edge, heading right: already at the exit plane.
        assert!(close(r.ray_exit_distance(Point::new(10.0, 5.0), Vec2::X), 0.0));
        // Starting at the centre: half the width.
        assert!(close(r.ray_exit_distance(Point::new(5.0, 5.0), Vec2::X), 5.0));
        // Approaching from outside: the far plane, so the stub lands clear.
        assert!(close(r.ray_exit_distance(Point::new(-2.0, 5.0), Vec2::X), 12.0));
    }

    #[test]
    fn polyline_drops_consecutive_duplicates() {
        let p = Polyline::new([
            Point::new(0.0, 0.0),
            Point::new(0.0, 0.0),
            Point::new(10.0, 0.0),
            Point::new(10.0, 0.0),
        ]);
        assert_eq!(p.len(), 2);
        assert!(close(p.length(), 10.0));
    }

    #[test]
    fn trimming_both_ends_shortens_by_exactly_that_much() {
        let p = Polyline::new([Point::new(0.0, 0.0), Point::new(100.0, 0.0)]);
        let t = p.trimmed(10.0, 20.0);
        assert!(close(t.length(), 70.0));
        assert_eq!(t.points.first(), Some(&Point::new(10.0, 0.0)));
        assert_eq!(t.points.last(), Some(&Point::new(80.0, 0.0)));
    }

    #[test]
    fn over_trimming_yields_nothing_rather_than_an_inverted_line() {
        let p = Polyline::new([Point::new(0.0, 0.0), Point::new(10.0, 0.0)]);
        assert!(p.trimmed(8.0, 8.0).is_empty());
    }

    #[test]
    fn trimming_spans_corners() {
        let p = Polyline::new([
            Point::new(0.0, 0.0),
            Point::new(10.0, 0.0),
            Point::new(10.0, 10.0),
        ]);
        let t = p.trimmed(5.0, 5.0);
        assert!(close(t.length(), 10.0));
        assert_eq!(t.points, vec![Point::new(5.0, 0.0), Point::new(10.0, 0.0), Point::new(10.0, 5.0)]);
    }

    #[test]
    fn distance_to_a_single_point_polyline_is_the_point_distance() {
        let p = Polyline::new([Point::new(4.0, 0.0)]);
        assert!(close(p.distance_to(Point::new(0.0, 0.0)), 4.0));
        assert!(Polyline::default().distance_to(Point::ORIGIN).is_infinite());
    }

    #[test]
    fn crossing_segments_report_their_intersection() {
        let hit = segment_intersection(
            Point::new(-5.0, 0.0),
            Point::new(5.0, 0.0),
            Point::new(0.0, -5.0),
            Point::new(0.0, 5.0),
        );
        assert_eq!(hit, Some(Point::ORIGIN));
    }

    #[test]
    fn segments_that_merely_touch_do_not_cross() {
        // Sharing an endpoint, as two connectors on the same widget anchor do.
        assert_eq!(
            segment_intersection(
                Point::ORIGIN,
                Point::new(100.0, 0.0),
                Point::ORIGIN,
                Point::new(0.0, 100.0),
            ),
            None
        );
        // A T-junction: one segment ends on the other's flank.
        assert_eq!(
            segment_intersection(
                Point::new(-50.0, 0.0),
                Point::new(50.0, 0.0),
                Point::new(0.0, -50.0),
                Point::ORIGIN,
            ),
            None
        );
    }

    #[test]
    fn parallel_and_non_overlapping_segments_do_not_cross() {
        assert_eq!(
            segment_intersection(
                Point::new(0.0, 0.0),
                Point::new(10.0, 0.0),
                Point::new(0.0, 1.0),
                Point::new(10.0, 1.0),
            ),
            None
        );
        assert_eq!(
            segment_intersection(
                Point::new(0.0, 0.0),
                Point::new(10.0, 0.0),
                Point::new(20.0, -5.0),
                Point::new(20.0, 5.0),
            ),
            None
        );
    }
}
