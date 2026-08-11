//! The geometry a table is laid out in: points, sizes, rectangles and insets.
//!
//! **`f64` world pixels**, matching `vellum-connect` and the world side of the
//! `f64 world → camera-relative f32` split in `docs/01-architecture.md` §3. A table
//! is a board widget: its cells are world rectangles that the camera transform turns
//! into `f32` late, per frame. Laying out in `f32` would put a table's rightmost
//! column boundary — the sum of every preceding width — at the far end of an error
//! chain, and a hairline that lands half a pixel off is exactly the artefact the
//! design language's "1px `frost` borders" cannot survive.
//!
//! **`+x` right, `+y` down**, as everywhere else in Vellum, so row 0 is the top row
//! and a rect's `origin` is its top-left corner.
//!
//! These types are deliberately local rather than borrowed from `vellum-scene` or
//! `vellum-connect`. This crate is pure logic with no GPU, no document and no
//! spatial index; depending on one of those for four field names would invert the
//! dependency direction §2 sets out. The conversion at either boundary is a struct
//! literal.

use serde::{Deserialize, Serialize};

/// Lengths below this are treated as zero.
///
/// Boards are tens of thousands of world pixels wide, so `1e-9` is far below any
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

/// A width and a height in world pixels. Never negative — every constructor
/// clamps, because a negative track size is a caller error that should show up as a
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
/// Origin-and-size rather than min-and-max because every consumer of this crate
/// wants the former — a renderer instances a quad from a corner and an extent, and
/// a cell rectangle is naturally "here, this big". The edges are one method away.
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
    /// Adjacent cells share a boundary coordinate exactly — there is no gap between
    /// columns, because the border is drawn *on* the boundary rather than between
    /// two of them — so a closed test would report two hits for a point on a shared
    /// edge. Half-open makes the boundary belong to the cell it starts.
    pub fn contains(self, p: Point) -> bool {
        p.x >= self.left() && p.x < self.right() && p.y >= self.top() && p.y < self.bottom()
    }

    /// Shrinks the rect by `insets`, collapsing to zero rather than inverting when
    /// the padding exceeds the cell. A 4px-wide column with 8px of padding is a
    /// legitimate transient state during a drag, and it must not produce a content
    /// rect with negative width that the text engine would then wrap against.
    pub fn inset(self, insets: Insets) -> Self {
        let left = self.left() + insets.left;
        let top = self.top() + insets.top;
        let right = (self.right() - insets.right).max(left);
        let bottom = (self.bottom() - insets.bottom).max(top);
        Self::from_edges(left, top, right, bottom)
    }
}

/// Per-side padding, in CSS order.
///
/// Defaults to a uniform 8px: the design language puts spacing on a 4px grid, and
/// 8 is the smallest step on that grid at which 13px body text does not touch the
/// hairline border of its own cell.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
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

    /// Vertical and horizontal padding — the pair a cell actually wants to vary,
    /// since dense tables tighten the rows without crowding the text against the
    /// column boundary.
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

impl Default for Insets {
    fn default() -> Self {
        Self::uniform(8.0)
    }
}

/// Clamps a length to a finite, non-negative value.
///
/// `NaN` collapses to zero rather than propagating: a single `NaN` width would
/// poison every prefix sum after it, so the whole table right of one bad cell would
/// vanish. A zero-width column is visibly wrong in one place, which is the failure
/// worth having.
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

    /// Adjacent cells share a boundary exactly, so the half-open rule is what stops
    /// a click on a column boundary from hitting two cells.
    #[test]
    fn containment_is_half_open_so_a_shared_edge_belongs_to_one_cell() {
        let left = Rect::new(0.0, 0.0, 100.0, 50.0);
        let right = Rect::new(100.0, 0.0, 100.0, 50.0);
        let on_the_seam = Point::new(100.0, 25.0);
        assert!(!left.contains(on_the_seam));
        assert!(right.contains(on_the_seam));
    }

    #[test]
    fn padding_larger_than_the_cell_collapses_instead_of_inverting() {
        let cell = Rect::new(10.0, 10.0, 4.0, 4.0);
        let content = cell.inset(Insets::uniform(8.0));
        assert_eq!(content.width(), 0.0);
        assert_eq!(content.height(), 0.0);
        assert_eq!(content.left(), 18.0);
    }

    #[test]
    fn insets_report_their_axis_totals() {
        let i = Insets::symmetric(4.0, 12.0);
        assert_eq!(i.vertical(), 8.0);
        assert_eq!(i.horizontal(), 24.0);
        assert_eq!(Insets::default(), Insets::uniform(8.0));
    }
}
