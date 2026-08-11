//! The geometry vocabulary a mind map is laid out in: points, vectors, sizes and
//! rectangles, all in `f64` world pixels.
//!
//! Deliberately local rather than borrowed from `vellum-scene` or `vellum-connect`,
//! for the reason `vellum-connect`'s own `geometry` module records: a pure-logic
//! crate pointing at the spatial-index crate would invert the dependency direction
//! in `docs/01-architecture.md` §2 for the sake of four field names, and the
//! conversion at the boundary is a struct literal. This crate stays buildable and
//! testable with nothing but `serde` behind it.
//!
//! **`+x` is right, `+y` is down**, matching Miro and the rest of Vellum. Every
//! angle in the radial layout is therefore measured *clockwise* from `+x` when read
//! off the screen, which is worth stating once here rather than rediscovering from a
//! mind map that fans out anticlockwise.

use serde::{Deserialize, Serialize};

/// Lengths below this are treated as zero.
///
/// Same value and same reasoning as `vellum-connect`: world space is `f64` px over
/// boards tens of thousands of px wide, so `1e-9` is far below any distance a user
/// can express and far above the accumulated error of the handful of multiply-adds
/// a layout performs per node.
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

    /// Linear interpolation. `t = 0` is `self`, `t = 1` is `other`.
    pub fn lerp(self, other: Self, t: f64) -> Self {
        Self::new(self.x + (other.x - self.x) * t, self.y + (other.y - self.y) * t)
    }

    pub fn is_finite(self) -> bool {
        self.x.is_finite() && self.y.is_finite()
    }

    /// True when two points are within [`EPSILON`] of each other.
    ///
    /// Used wherever coincidence decides control flow — chiefly when a connector's
    /// two endpoints land on the same spot and a polyline through both would be a
    /// zero-length segment for the tessellator to choke on.
    pub fn coincident_with(self, other: Self) -> bool {
        (other - self).length() <= EPSILON
    }
}

/// A displacement between two points. Separate from [`Point`] because this crate's
/// stability machinery is entirely about *offsets* — "how far did this node move" is
/// the question the whole of [`Layout::stabilise_against`](crate::Layout::stabilise_against)
/// exists to answer — and a translation applied to a translation is a bug that a
/// distinct type catches for free.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct Vec2 {
    pub x: f64,
    pub y: f64,
}

impl Vec2 {
    pub const ZERO: Self = Self { x: 0.0, y: 0.0 };

    pub const fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }

    pub fn length(self) -> f64 {
        self.x.hypot(self.y)
    }

    /// Unit vector, or [`Vec2::ZERO`] for a degenerate input.
    ///
    /// Returning zero rather than `NaN` matters here: a connector between two nodes
    /// that happen to share a centre would otherwise poison every downstream
    /// comparison instead of producing one visibly wrong link.
    pub fn normalised(self) -> Self {
        let len = self.length();
        if len <= EPSILON { Self::ZERO } else { Self::new(self.x / len, self.y / len) }
    }

    /// Sum of the absolute components — the L1 (taxicab) length.
    ///
    /// The measure [`Layout::stabilise_against`](crate::Layout::stabilise_against)
    /// minimises. L1 rather than L2 because its minimum is attained *at an observed
    /// displacement* — the componentwise median — so the offset tends to leave a
    /// block of nodes exactly where they were. Minimising L2 would instead nudge
    /// everything a little, which is the opposite of what "do not lose your place"
    /// means: the eye tracks one node that did not move far better than a whole map
    /// that moved slightly.
    pub fn manhattan_length(self) -> f64 {
        self.x.abs() + self.y.abs()
    }
}

impl std::ops::Add<Vec2> for Point {
    type Output = Point;
    fn add(self, rhs: Vec2) -> Point {
        Point::new(self.x + rhs.x, self.y + rhs.y)
    }
}

impl std::ops::Sub<Vec2> for Point {
    type Output = Point;
    fn sub(self, rhs: Vec2) -> Point {
        Point::new(self.x - rhs.x, self.y - rhs.y)
    }
}

impl std::ops::Sub<Point> for Point {
    type Output = Vec2;
    fn sub(self, rhs: Point) -> Vec2 {
        Vec2::new(self.x - rhs.x, self.y - rhs.y)
    }
}

impl std::ops::Add for Vec2 {
    type Output = Self;
    fn add(self, rhs: Self) -> Self {
        Self::new(self.x + rhs.x, self.y + rhs.y)
    }
}

impl std::ops::Neg for Vec2 {
    type Output = Self;
    fn neg(self) -> Self {
        Self::new(-self.x, -self.y)
    }
}

impl std::ops::Mul<f64> for Vec2 {
    type Output = Self;
    fn mul(self, rhs: f64) -> Self {
        Self::new(self.x * rhs, self.y * rhs)
    }
}

/// The extent of a node's box, in world pixels.
///
/// This crate never measures text. `vellum-text` shapes a node's label and the
/// caller writes the result here, exactly as it does for stickies — layout is a
/// geometry problem given sizes, and pulling `cosmic-text` into it would make the
/// whole of this crate untestable on a machine with no font stack.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Size {
    pub width: f64,
    pub height: f64,
}

impl Size {
    pub const ZERO: Self = Self { width: 0.0, height: 0.0 };

    pub const fn new(width: f64, height: f64) -> Self {
        Self { width, height }
    }

    /// The diagonal of the box.
    ///
    /// Used by the radial layout as a node's separation radius: a box rotated to any
    /// angle still fits inside a circle of this diameter, so separating two nodes by
    /// the mean of their diagonals is safe whatever angle the layout lands them at.
    /// The radial layout in [`crate::layout`] explains why that conservatism is
    /// unavoidable, and proves that it makes the non-overlap guarantee exact.
    pub fn diagonal(self) -> f64 {
        self.width.hypot(self.height)
    }

    /// Both extents clamped to be non-negative and finite.
    ///
    /// A `NaN` size arriving from a text measurement that divided by a zero font
    /// size would otherwise propagate through every comparison in the tidy pass and
    /// silently produce a map with every node at the origin. Sanitising once, at the
    /// point sizes enter layout, is far cheaper than defending each comparison.
    pub fn sanitised(self) -> Self {
        let fix = |v: f64| if v.is_finite() && v > 0.0 { v } else { 0.0 };
        Self::new(fix(self.width), fix(self.height))
    }
}

impl Default for Size {
    /// The default node box: 160 × 44 px.
    ///
    /// Sized to hold a short label at the 13px UI body size of
    /// `docs/05-design-language.md` §5 with the 4px-grid padding that document asks
    /// for, so a map built entirely from `Node::new` looks deliberate before anyone
    /// has measured a single string.
    fn default() -> Self {
        Self::new(160.0, 44.0)
    }
}

/// An axis-aligned box. `min` is the top-left corner, because y runs downwards.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Rect {
    pub min: Point,
    pub max: Point,
}

impl Rect {
    pub const fn new(min: Point, max: Point) -> Self {
        Self { min, max }
    }

    /// The box of the given size centred on `centre`. This is how every node rect in
    /// a [`Layout`](crate::Layout) is built: the layout algorithms work in centres,
    /// because a tidy tree separates nodes by the *mean* of their extents and that
    /// arithmetic only reads naturally about centres.
    pub fn from_centre(centre: Point, size: Size) -> Self {
        let (hw, hh) = (size.width * 0.5, size.height * 0.5);
        Self::new(
            Point::new(centre.x - hw, centre.y - hh),
            Point::new(centre.x + hw, centre.y + hh),
        )
    }

    pub fn centre(self) -> Point {
        Point::new((self.min.x + self.max.x) * 0.5, (self.min.y + self.max.y) * 0.5)
    }

    pub fn width(self) -> f64 {
        self.max.x - self.min.x
    }

    pub fn height(self) -> f64 {
        self.max.y - self.min.y
    }

    pub fn size(self) -> Size {
        Size::new(self.width(), self.height())
    }

    /// Inclusive containment, which is what hit-testing wants: a click exactly on a
    /// node's border should select it rather than fall through to the canvas.
    pub fn contains(self, p: Point) -> bool {
        p.x >= self.min.x && p.x <= self.max.x && p.y >= self.min.y && p.y <= self.max.y
    }

    /// Strict overlap: boxes that merely share an edge do **not** intersect.
    ///
    /// The distinction is the whole point of the overlap tests in this crate. A tidy
    /// layout with a zero gap legitimately places two boxes edge to edge, and a test
    /// that called that an overlap would fail on correct output.
    pub fn intersects(self, other: Self) -> bool {
        self.min.x < other.max.x
            && other.min.x < self.max.x
            && self.min.y < other.max.y
            && other.min.y < self.max.y
    }

    pub fn union(self, other: Self) -> Self {
        Self::new(
            Point::new(self.min.x.min(other.min.x), self.min.y.min(other.min.y)),
            Point::new(self.max.x.max(other.max.x), self.max.y.max(other.max.y)),
        )
    }

    pub fn translated(self, by: Vec2) -> Self {
        Self::new(self.min + by, self.max + by)
    }

    /// Where a straight line from this box's centre towards `target` leaves the box,
    /// together with the outward unit normal of the edge it leaves through.
    ///
    /// This is how a connector finds its attachment point, and it is done
    /// geometrically rather than by picking a named side so that it works unchanged
    /// for the radial layout, where a child can sit at any angle from its parent.
    /// The normal is what the elbow and curve forms need in order to leave a node
    /// perpendicular to its edge instead of at whatever angle the centres happen to
    /// make — which is the difference between a diagram and a spider's web.
    ///
    /// A `target` at this box's own centre has no well-defined direction; the right
    /// edge is returned, because a degenerate link should still be a drawable
    /// two-point path rather than a `NaN`.
    ///
    /// The returned point is **exactly** on the border and never a rounding step
    /// outside it: the coordinate the chosen edge pins is written directly rather
    /// than reconstructed as `centre + dir · t`, and the free coordinate is clamped
    /// to the box. Callers legitimately assert that a connector's endpoint is inside
    /// the node it attaches to, and one ulp of slack would make that assertion
    /// flicker.
    pub fn boundary_towards(self, target: Point) -> (Point, Vec2) {
        let centre = self.centre();
        let dir = target - centre;
        let (hw, hh) = (self.width() * 0.5, self.height() * 0.5);
        let tx = if dir.x.abs() > EPSILON { hw / dir.x.abs() } else { f64::INFINITY };
        let ty = if dir.y.abs() > EPSILON { hh / dir.y.abs() } else { f64::INFINITY };
        if !tx.is_finite() && !ty.is_finite() {
            return (Point::new(self.max.x, centre.y), Vec2::new(1.0, 0.0));
        }
        if tx <= ty {
            // `|dir.x| > EPSILON` here, or `tx` would be infinite and this branch
            // unreachable, so the sign is a real ±1.
            let sign = dir.x.signum();
            let x = if sign > 0.0 { self.max.x } else { self.min.x };
            let y = (centre.y + dir.y * tx).clamp(self.min.y, self.max.y);
            (Point::new(x, y), Vec2::new(sign, 0.0))
        } else {
            let sign = dir.y.signum();
            let x = (centre.x + dir.x * ty).clamp(self.min.x, self.max.x);
            let y = if sign > 0.0 { self.max.y } else { self.min.y };
            (Point::new(x, y), Vec2::new(0.0, sign))
        }
    }

    /// The bounds of a set of boxes, or `None` for an empty set.
    pub fn of(boxes: impl IntoIterator<Item = Rect>) -> Option<Self> {
        let mut iter = boxes.into_iter();
        let first = iter.next()?;
        Some(iter.fold(first, Rect::union))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boxes_sharing_an_edge_do_not_intersect() {
        let a = Rect::new(Point::new(0.0, 0.0), Point::new(10.0, 10.0));
        let touching = Rect::new(Point::new(10.0, 0.0), Point::new(20.0, 10.0));
        let overlapping = Rect::new(Point::new(9.9, 0.0), Point::new(20.0, 10.0));
        assert!(!a.intersects(touching), "a zero gap is a legal tidy layout, not an overlap");
        assert!(a.intersects(overlapping));
    }

    #[test]
    fn boundary_towards_leaves_through_the_facing_edge() {
        let r = Rect::from_centre(Point::ORIGIN, Size::new(100.0, 40.0));

        let (p, n) = r.boundary_towards(Point::new(500.0, 0.0));
        assert_eq!(p, Point::new(50.0, 0.0));
        assert_eq!(n, Vec2::new(1.0, 0.0));

        // Straight down: the box is wide and short, so the horizontal edge wins even
        // though the horizontal half-extent is larger.
        let (p, n) = r.boundary_towards(Point::new(0.0, 500.0));
        assert_eq!(p, Point::new(0.0, 20.0));
        assert_eq!(n, Vec2::new(0.0, 1.0));

        // Diagonal far enough off-axis to leave through the top edge.
        let (p, n) = r.boundary_towards(Point::new(-30.0, -300.0));
        assert!(p.y < -19.99 && p.x < 0.0);
        assert_eq!(n, Vec2::new(0.0, -1.0));
    }

    #[test]
    fn boundary_towards_own_centre_is_finite() {
        let r = Rect::from_centre(Point::new(5.0, 5.0), Size::new(10.0, 10.0));
        let (p, n) = r.boundary_towards(r.centre());
        assert!(p.is_finite());
        assert_eq!(n, Vec2::new(1.0, 0.0));
    }

    #[test]
    fn sizes_are_sanitised_rather_than_propagating_nan() {
        assert_eq!(Size::new(f64::NAN, 10.0).sanitised(), Size::new(0.0, 10.0));
        assert_eq!(Size::new(-4.0, f64::INFINITY).sanitised(), Size::ZERO);
        assert_eq!(Size::new(3.0, 4.0).sanitised().diagonal(), 5.0);
    }

    #[test]
    fn manhattan_length_is_the_measure_stabilisation_minimises() {
        assert_eq!(Vec2::new(-3.0, 4.0).manhattan_length(), 7.0);
        assert_eq!(Vec2::ZERO.normalised(), Vec2::ZERO);
    }
}
