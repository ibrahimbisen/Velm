//! The two coordinate spaces Vellum works in, kept as distinct types.
//!
//! World space and screen space are both "a pair of numbers", and mixing them up
//! produces a board that looks *almost* right — the failure mode that is hardest to
//! spot in a screenshot and hardest to debug once it ships. Separate types make the
//! mistake a compile error instead.
//!
//! World space is `f64` because the reference board measures 41282 × 17515 px and a
//! user can pan far beyond it; screen space is `f64` too, because winit reports
//! cursor positions as `f64` physical pixels and rounding early loses trackpad
//! sub-pixel deltas.

/// A point on the board, in world pixels. `+x` is right, `+y` is down — the same
/// orientation Miro uses, so imported `_position.offsetPx` values map across
/// unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct WorldPoint {
    pub x: f64,
    pub y: f64,
}

impl WorldPoint {
    pub const ORIGIN: Self = Self { x: 0.0, y: 0.0 };

    pub const fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }

    /// The rstar representation. rstar indexes `[f64; 2]` natively, so this is the
    /// single conversion point between our named type and the index's tuple form.
    pub const fn to_array(self) -> [f64; 2] {
        [self.x, self.y]
    }
}

/// A point in the window, in *physical* pixels, origin at the top-left.
///
/// Physical rather than logical: everything downstream (the surface, the viewport,
/// the wgpu scissor rect) is in physical pixels, so converting once at the window
/// boundary keeps a Retina scale factor from leaking into the camera math.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct ScreenPoint {
    pub x: f64,
    pub y: f64,
}

impl ScreenPoint {
    pub const fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }
}

/// A window size in physical pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScreenSize {
    pub width: f64,
    pub height: f64,
}

impl ScreenSize {
    pub const fn new(width: f64, height: f64) -> Self {
        Self { width, height }
    }
}

/// An axis-aligned rectangle in world space, stored as its two extreme corners.
///
/// Constructors normalise so `min <= max` on both axes. An unnormalised rect would
/// silently intersect nothing, which reads as "the item vanished" rather than as a
/// bug in whatever produced the rect.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WorldRect {
    pub min: WorldPoint,
    pub max: WorldPoint,
}

impl WorldRect {
    /// Builds a rect from any two opposite corners, in either order.
    pub fn from_corners(a: WorldPoint, b: WorldPoint) -> Self {
        Self {
            min: WorldPoint::new(a.x.min(b.x), a.y.min(b.y)),
            max: WorldPoint::new(a.x.max(b.x), a.y.max(b.y)),
        }
    }

    /// Builds a rect from its top-left corner and a size. Negative sizes are
    /// normalised, so a drag that goes up-and-left still produces a valid rect.
    pub fn from_origin_size(origin: WorldPoint, width: f64, height: f64) -> Self {
        Self::from_corners(origin, WorldPoint::new(origin.x + width, origin.y + height))
    }

    pub fn width(&self) -> f64 {
        self.max.x - self.min.x
    }

    pub fn height(&self) -> f64 {
        self.max.y - self.min.y
    }

    pub fn center(&self) -> WorldPoint {
        WorldPoint::new(
            self.min.x + self.width() / 2.0,
            self.min.y + self.height() / 2.0,
        )
    }

    /// Edge-inclusive containment. Inclusive so that clicking the exact border of a
    /// sticky selects it rather than falling through to whatever is behind.
    pub fn contains(&self, p: WorldPoint) -> bool {
        p.x >= self.min.x && p.x <= self.max.x && p.y >= self.min.y && p.y <= self.max.y
    }

    /// Edge-inclusive overlap test, matching the R-tree's own envelope semantics so
    /// a brute-force check and an indexed query can never disagree.
    pub fn intersects(&self, other: &Self) -> bool {
        self.min.x <= other.max.x
            && self.max.x >= other.min.x
            && self.min.y <= other.max.y
            && self.max.y >= other.min.y
    }

    /// Grows the rect outwards on every side. Used to pad a viewport query so items
    /// that are partly off-screen still take part in the frame.
    pub fn inflate(&self, amount: f64) -> Self {
        Self {
            min: WorldPoint::new(self.min.x - amount, self.min.y - amount),
            max: WorldPoint::new(self.max.x + amount, self.max.y + amount),
        }
    }

    pub fn to_aabb(self) -> rstar::AABB<[f64; 2]> {
        rstar::AABB::from_corners(self.min.to_array(), self.max.to_array())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corners_are_normalised_in_either_order() {
        let a = WorldRect::from_corners(WorldPoint::new(10.0, 20.0), WorldPoint::new(-5.0, 3.0));
        assert_eq!(a.min, WorldPoint::new(-5.0, 3.0));
        assert_eq!(a.max, WorldPoint::new(10.0, 20.0));
        assert_eq!(a.width(), 15.0);
        assert_eq!(a.height(), 17.0);
    }

    #[test]
    fn negative_sizes_still_produce_a_valid_rect() {
        let r = WorldRect::from_origin_size(WorldPoint::new(0.0, 0.0), -4.0, -6.0);
        assert_eq!(r.min, WorldPoint::new(-4.0, -6.0));
        assert_eq!(r.max, WorldPoint::ORIGIN);
    }

    #[test]
    fn containment_includes_the_border() {
        let r = WorldRect::from_origin_size(WorldPoint::new(0.0, 0.0), 10.0, 10.0);
        assert!(r.contains(WorldPoint::new(0.0, 0.0)));
        assert!(r.contains(WorldPoint::new(10.0, 10.0)));
        assert!(r.contains(WorldPoint::new(5.0, 5.0)));
        assert!(!r.contains(WorldPoint::new(10.001, 5.0)));
        assert!(!r.contains(WorldPoint::new(5.0, -0.001)));
    }

    #[test]
    fn intersection_is_symmetric_and_edge_inclusive() {
        let a = WorldRect::from_origin_size(WorldPoint::new(0.0, 0.0), 10.0, 10.0);
        let touching = WorldRect::from_origin_size(WorldPoint::new(10.0, 0.0), 5.0, 5.0);
        let apart = WorldRect::from_origin_size(WorldPoint::new(10.001, 0.0), 5.0, 5.0);

        assert!(a.intersects(&touching) && touching.intersects(&a));
        assert!(!a.intersects(&apart) && !apart.intersects(&a));
    }

    #[test]
    fn center_is_the_midpoint() {
        let r = WorldRect::from_corners(WorldPoint::new(-10.0, 4.0), WorldPoint::new(30.0, 8.0));
        assert_eq!(r.center(), WorldPoint::new(10.0, 6.0));
    }

    #[test]
    fn inflate_grows_every_side() {
        let r = WorldRect::from_origin_size(WorldPoint::new(0.0, 0.0), 10.0, 10.0).inflate(2.0);
        assert_eq!(r.min, WorldPoint::new(-2.0, -2.0));
        assert_eq!(r.max, WorldPoint::new(12.0, 12.0));
    }
}
