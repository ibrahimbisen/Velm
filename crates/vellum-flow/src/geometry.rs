//! The space a container is laid out in: points, sizes, rectangles and insets.
//!
//! **`f64` world pixels**, matching the world side of the `f64 world →
//! camera-relative f32` split in `docs/01-architecture.md` §3. A container is a
//! board widget: its children are world rectangles that the camera turns into `f32`
//! late, once per frame. Laying out in `f32` would put the rightmost column boundary
//! — the sum of every preceding width — at the end of an error chain, and a hairline
//! that lands half a pixel off is exactly what the design language's 1px `frost`
//! borders cannot survive.
//!
//! **`+x` right, `+y` down**, so the first column is leftmost, the first release row
//! is topmost, and a rect's `origin` is its top-left corner.
//!
//! These types are local rather than borrowed from `vellum-scene` or a sibling
//! widget crate. This crate is pure logic — no GPU, no document, no spatial index —
//! and depending on one of those for four field names would invert the dependency
//! direction `docs/01-architecture.md` §2 sets out. The conversion at either
//! boundary is a struct literal.

use serde::{Deserialize, Serialize};

/// Lengths below this are treated as zero.
///
/// Boards are tens of thousands of world pixels across, so `1e-9` is far below any
/// distance a user can express and far above the accumulated error of the prefix
/// sums that produce a column boundary.
pub const EPSILON: f64 = 1e-9;

/// A point in world pixels.
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

    pub fn translated(self, dx: f64, dy: f64) -> Self {
        Self::new(self.x + dx, self.y + dy)
    }
}

/// A width and a height in world pixels. Never negative — every constructor clamps,
/// because a negative track size is a caller error that should surface as a
/// collapsed column rather than as an inside-out rectangle five layers downstream.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct Size {
    pub width: f64,
    pub height: f64,
}

impl Size {
    pub const ZERO: Self = Self { width: 0.0, height: 0.0 };

    pub fn new(width: f64, height: f64) -> Self {
        Self { width: sanitise(width), height: sanitise(height) }
    }

    /// True when the size encloses no area, and therefore nothing to draw.
    pub fn is_empty(self) -> bool {
        self.width <= EPSILON || self.height <= EPSILON
    }
}

/// An axis-aligned rectangle: top-left corner plus extent.
///
/// Origin-and-size rather than min-and-max because that is what every consumer
/// wants — a renderer instances a quad from a corner and an extent, and a card
/// rectangle is naturally "here, this big". The edges are one method away.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct Rect {
    pub origin: Point,
    pub size: Size,
}

impl Rect {
    pub const ZERO: Self = Self { origin: Point::ORIGIN, size: Size::ZERO };

    pub fn new(x: f64, y: f64, width: f64, height: f64) -> Self {
        Self { origin: Point::new(x, y), size: Size::new(width, height) }
    }

    /// Builds a rect from its four edges, tolerating them arriving inverted: a
    /// `right` left of `left` produces a zero-width rect rather than a negative one.
    pub fn from_edges(left: f64, top: f64, right: f64, bottom: f64) -> Self {
        Self::new(left, top, right - left, bottom - top)
    }

    pub fn left(self) -> f64 {
        self.origin.x
    }

    pub fn top(self) -> f64 {
        self.origin.y
    }

    pub fn right(self) -> f64 {
        self.origin.x + self.size.width
    }

    pub fn bottom(self) -> f64 {
        self.origin.y + self.size.height
    }

    pub fn width(self) -> f64 {
        self.size.width
    }

    pub fn height(self) -> f64 {
        self.size.height
    }

    pub fn centre(self) -> Point {
        Point::new(self.left() + self.size.width * 0.5, self.top() + self.size.height * 0.5)
    }

    pub fn is_empty(self) -> bool {
        self.size.is_empty()
    }

    /// Half-open containment: inclusive on the top and left edges, exclusive on the
    /// bottom and right.
    ///
    /// Columns in a kanban and cells in a story map are laid out edge to edge, so a
    /// closed test would report two hits for a point on a shared boundary. Half-open
    /// makes the boundary belong to the cell it starts.
    pub fn contains(self, p: Point) -> bool {
        p.x >= self.left() && p.x < self.right() && p.y >= self.top() && p.y < self.bottom()
    }

    /// Containment on the x axis alone.
    ///
    /// A drag over a kanban targets a column by horizontal position: releasing above
    /// the first card or below the last must still drop into that column rather than
    /// falling through to the container, so the y coordinate is deliberately not part
    /// of the test.
    pub fn contains_x(self, x: f64) -> bool {
        x >= self.left() && x < self.right()
    }

    /// Shrinks the rect on every side, collapsing to zero rather than inverting when
    /// the padding exceeds the rect. A column narrower than its own padding is a
    /// legitimate transient state while an area is being resized, and it must not
    /// produce a negative-width content rect.
    pub fn inset_by(self, amount: f64) -> Self {
        self.inset(Insets::uniform(amount))
    }

    /// As [`Rect::inset_by`], per side.
    pub fn inset(self, insets: Insets) -> Self {
        let left = self.left() + insets.left;
        let top = self.top() + insets.top;
        let right = (self.right() - insets.right).max(left);
        let bottom = (self.bottom() - insets.bottom).max(top);
        Self::from_edges(left, top, right, bottom)
    }

    /// Splits `height` off the top, returning the strip and what is left below it.
    ///
    /// Container chrome is a stack of strips — title, then axis, then body — and
    /// writing it this way means the remainder is computed once rather than each
    /// strip re-deriving its own offset from the top of the container.
    pub fn split_top(self, height: f64) -> (Self, Self) {
        let height = sanitise(height).min(self.height());
        let strip = Self::new(self.left(), self.top(), self.width(), height);
        let rest = Self::from_edges(self.left(), self.top() + height, self.right(), self.bottom());
        (strip, rest)
    }

    /// Splits `width` off the left, returning the strip and what is left to its
    /// right. The story map's release rail is exactly this.
    pub fn split_left(self, width: f64) -> (Self, Self) {
        let width = sanitise(width).min(self.width());
        let strip = Self::new(self.left(), self.top(), width, self.height());
        let rest = Self::from_edges(self.left() + width, self.top(), self.right(), self.bottom());
        (strip, rest)
    }
}

/// Per-side padding, in CSS order.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct Insets {
    pub top: f64,
    pub right: f64,
    pub bottom: f64,
    pub left: f64,
}

impl Insets {
    pub const ZERO: Self = Self { top: 0.0, right: 0.0, bottom: 0.0, left: 0.0 };

    pub fn uniform(all: f64) -> Self {
        let all = sanitise(all);
        Self { top: all, right: all, bottom: all, left: all }
    }

    /// Vertical and horizontal padding — the pair a container actually varies, since
    /// a dense board tightens its rows without crowding text against a column edge.
    pub fn symmetric(vertical: f64, horizontal: f64) -> Self {
        let v = sanitise(vertical);
        let h = sanitise(horizontal);
        Self { top: v, right: h, bottom: v, left: h }
    }

    pub fn horizontal(self) -> f64 {
        self.left + self.right
    }

    pub fn vertical(self) -> f64 {
        self.top + self.bottom
    }
}

/// Clamps a length to a finite, non-negative value.
///
/// `NaN` collapses to zero rather than propagating: one `NaN` height would poison
/// every prefix sum after it, so every card below one bad card would vanish. A
/// zero-height card is visibly wrong in one place, which is the failure worth having.
pub(crate) fn sanitise(v: f64) -> f64 {
    if v.is_finite() && v > 0.0 { v } else { 0.0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negative_and_nan_extents_collapse_rather_than_invert() {
        assert_eq!(Size::new(-5.0, 10.0), Size::new(0.0, 10.0));
        assert_eq!(Size::new(f64::NAN, f64::INFINITY), Size::ZERO);
        let r = Rect::from_edges(100.0, 0.0, 40.0, 20.0);
        assert_eq!(r.width(), 0.0);
        assert_eq!(r.left(), 100.0);
    }

    /// Adjacent columns share a boundary exactly, so the half-open rule is what
    /// stops a click on the seam from hitting both of them.
    #[test]
    fn containment_is_half_open_so_a_shared_edge_belongs_to_one_rect() {
        let left = Rect::new(0.0, 0.0, 100.0, 50.0);
        let right = Rect::new(100.0, 0.0, 100.0, 50.0);
        let seam = Point::new(100.0, 25.0);
        assert!(!left.contains(seam));
        assert!(right.contains(seam));
        assert!(!left.contains_x(100.0));
        assert!(right.contains_x(100.0));
    }

    #[test]
    fn padding_larger_than_the_rect_collapses_instead_of_inverting() {
        let cell = Rect::new(10.0, 10.0, 4.0, 4.0);
        let content = cell.inset_by(8.0);
        assert_eq!(content.width(), 0.0);
        assert_eq!(content.height(), 0.0);
        assert_eq!(content.left(), 18.0);
    }

    #[test]
    fn splitting_never_overruns_the_rect_it_splits() {
        let r = Rect::new(0.0, 0.0, 200.0, 100.0);
        let (strip, rest) = r.split_top(40.0);
        assert_eq!(strip, Rect::new(0.0, 0.0, 200.0, 40.0));
        assert_eq!(rest, Rect::new(0.0, 40.0, 200.0, 60.0));

        // A strip taller than the rect takes all of it and leaves nothing, rather
        // than producing a negative remainder below the bottom edge.
        let (strip, rest) = r.split_top(500.0);
        assert_eq!(strip, r);
        assert!(rest.is_empty());
        assert_eq!(rest.top(), 100.0);

        let (rail, body) = r.split_left(60.0);
        assert_eq!(rail.width(), 60.0);
        assert_eq!(body, Rect::new(60.0, 0.0, 140.0, 100.0));
    }

    #[test]
    fn insets_report_their_axis_totals() {
        let i = Insets::symmetric(4.0, 12.0);
        assert_eq!(i.vertical(), 8.0);
        assert_eq!(i.horizontal(), 24.0);
    }
}
