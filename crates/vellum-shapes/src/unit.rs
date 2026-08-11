//! The unit box — the coordinate system every shape in this crate is defined in.
//!
//! A shape is a *description*, not a placement: the same rectangle serves a 40px
//! sticky outline and a 4000px frame. So every shape is built inside a box whose
//! `x` and `y` both run `0..1`, **y downwards**, and the caller maps that box onto
//! the item's world rect. This is also the space Miro binds connectors in — an
//! anchor of `{x: 1, y: 0.5}` is the right edge, vertically centred — so anchors
//! need no conversion at all.
//!
//! `f32` rather than the `f64` used for world space in `vellum-doc`: unit
//! coordinates never leave `0..1`, where `f32` resolves to ~1e-7, and the
//! tessellated output is fed straight to the GPU as `f32`. Precision is spent
//! where it is needed — the world transform — not here.

use serde::{Deserialize, Serialize};

/// A point in the unit box, or a vector between two of them.
///
/// One type serves both roles deliberately: the geometry code below is full of
/// `b - a` differences that are immediately added back to another point, and a
/// separate `Vec2` would double the surface without catching a real class of bug.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct Point {
    pub x: f32,
    pub y: f32,
}

/// Shorthand for [`Point::new`], because shape definitions are almost entirely
/// point literals and `p(0.5, 0.0)` reads as geometry where the struct form reads
/// as bookkeeping.
pub const fn p(x: f32, y: f32) -> Point {
    Point::new(x, y)
}

impl Point {
    pub const fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }

    pub fn length(self) -> f32 {
        self.x.hypot(self.y)
    }

    pub fn distance(self, other: Self) -> f32 {
        (self - other).length()
    }

    pub fn dot(self, other: Self) -> f32 {
        self.x * other.x + self.y * other.y
    }

    /// The 2D cross product `self × other`, i.e. the z of the 3D cross product.
    /// Its sign says which side of `self` the vector `other` falls on, which is
    /// what every point-in-polygon and orientation test below is built from.
    pub fn cross(self, other: Self) -> f32 {
        self.x * other.y - self.y * other.x
    }

    /// Unit vector, or the zero vector if `self` is degenerate. Returning zero
    /// rather than `NaN` matters: a degenerate edge in a shape definition should
    /// produce a wrong-looking shape, not poison every downstream comparison.
    pub fn normalised(self) -> Self {
        let len = self.length();
        if len <= f32::EPSILON { Self::default() } else { self * (1.0 / len) }
    }

    pub(crate) fn to_lyon(self) -> lyon::math::Point {
        lyon::math::point(self.x, self.y)
    }

    pub(crate) fn from_lyon(p: lyon::math::Point) -> Self {
        Self::new(p.x, p.y)
    }
}

impl std::ops::Add for Point {
    type Output = Self;
    fn add(self, rhs: Self) -> Self {
        Self::new(self.x + rhs.x, self.y + rhs.y)
    }
}

impl std::ops::Sub for Point {
    type Output = Self;
    fn sub(self, rhs: Self) -> Self {
        Self::new(self.x - rhs.x, self.y - rhs.y)
    }
}

impl std::ops::Mul<f32> for Point {
    type Output = Self;
    fn mul(self, rhs: f32) -> Self {
        Self::new(self.x * rhs, self.y * rhs)
    }
}

impl std::ops::Neg for Point {
    type Output = Self;
    fn neg(self) -> Self {
        Self::new(-self.x, -self.y)
    }
}

/// An axis-aligned box. Used for shape bounds, where `min` is the top-left corner
/// because y runs downwards.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Bounds {
    pub min: Point,
    pub max: Point,
}

impl Bounds {
    /// The unit box itself. Every shape's [`bounds`](crate::Outline::bounds) should
    /// equal this to within rounding — shapes are normalised to fill their box so
    /// that a caller can map the box onto an item rect and get the size it asked
    /// for, with no shape-specific padding.
    pub const UNIT: Self = Self { min: Point::new(0.0, 0.0), max: Point::new(1.0, 1.0) };

    pub const fn new(min: Point, max: Point) -> Self {
        Self { min, max }
    }

    /// The bounds of a point set, or `None` for an empty one.
    pub fn of(points: impl IntoIterator<Item = Point>) -> Option<Self> {
        let mut iter = points.into_iter();
        let first = iter.next()?;
        let mut bounds = Self::new(first, first);
        for p in iter {
            bounds = bounds.expanded_to(p);
        }
        Some(bounds)
    }

    pub fn expanded_to(self, p: Point) -> Self {
        Self::new(
            Point::new(self.min.x.min(p.x), self.min.y.min(p.y)),
            Point::new(self.max.x.max(p.x), self.max.y.max(p.y)),
        )
    }

    pub fn union(self, other: Self) -> Self {
        self.expanded_to(other.min).expanded_to(other.max)
    }

    pub fn width(self) -> f32 {
        self.max.x - self.min.x
    }

    pub fn height(self) -> f32 {
        self.max.y - self.min.y
    }

    pub fn centre(self) -> Point {
        Point::new((self.min.x + self.max.x) * 0.5, (self.min.y + self.max.y) * 0.5)
    }

    /// Inclusive containment. Only ever used as a cheap reject before the real
    /// point-in-shape test — a bounding-box hit is not a shape hit, and for a star
    /// or a cross most of the box is not the shape.
    pub fn contains(self, p: Point) -> bool {
        p.x >= self.min.x && p.x <= self.max.x && p.y >= self.min.y && p.y <= self.max.y
    }
}

/// The size of the box a shape is being drawn into, in the caller's own units
/// (world units, or pixels — the shape code only ever uses the ratio).
///
/// Needed because a handful of shapes are not scale-invariant: a terminator's
/// semicircular ends and a rounded rectangle's corners must stay circular when the
/// box is stretched, so their unit-box definition depends on the aspect ratio.
/// Everything else ignores it.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Size {
    pub width: f32,
    pub height: f32,
}

impl Size {
    pub const SQUARE: Self = Self { width: 1.0, height: 1.0 };

    pub const fn new(width: f32, height: f32) -> Self {
        Self { width, height }
    }

    /// Width over height. Degenerate sizes report `1.0` rather than `inf`/`NaN`,
    /// so a zero-height item still produces a drawable shape instead of a path
    /// full of `NaN` that would take the tessellator down with it.
    pub fn aspect(self) -> f32 {
        if self.width > 0.0 && self.height > 0.0 { self.width / self.height } else { 1.0 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cross_sign_reports_which_side_a_vector_falls_on() {
        let east = p(1.0, 0.0);
        assert!(east.cross(p(0.0, 1.0)) > 0.0, "y-down: +y is clockwise from +x");
        assert!(east.cross(p(0.0, -1.0)) < 0.0);
        assert_eq!(east.cross(p(2.0, 0.0)), 0.0);
    }

    #[test]
    fn normalising_a_zero_vector_yields_zero_not_nan() {
        assert_eq!(Point::default().normalised(), Point::default());
        let n = p(3.0, 4.0).normalised();
        assert!((n.length() - 1.0).abs() < 1e-6);
    }

    #[test]
    fn bounds_of_a_point_set() {
        let b = Bounds::of([p(0.2, 0.9), p(-0.1, 0.4), p(0.5, 0.5)]).unwrap();
        assert_eq!(b.min, p(-0.1, 0.4));
        assert_eq!(b.max, p(0.5, 0.9));
        assert_eq!(b.centre(), p(0.2, 0.65));
        assert!(Bounds::of([]).is_none());
    }

    #[test]
    fn degenerate_sizes_report_a_square_aspect_rather_than_infinity() {
        assert_eq!(Size::new(2.0, 1.0).aspect(), 2.0);
        assert_eq!(Size::new(0.0, 1.0).aspect(), 1.0);
        assert_eq!(Size::new(1.0, 0.0).aspect(), 1.0);
    }
}
