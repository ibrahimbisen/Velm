//! Connector attachment points.
//!
//! Miro binds a connector end to a widget plus a normalised point on it — the
//! importer already decodes `{x: 1, y: 0.5}` as "right edge, vertically centred"
//! (see `vellum_import::miro_model::ConnectorEnd`). Because this crate's unit box
//! *is* that normalised space, an anchor needs no conversion; it needs resolving,
//! which is a different problem:
//!
//! **A normalised point is a point in the box, not a point on the shape.** On a
//! rectangle the two coincide, which is why the distinction is easy to miss. On a
//! diamond, `{1, 0}` is the top-right corner of the box and lies outside the shape
//! entirely; a connector drawn to it would end in mid-air, detached from the thing
//! it points at. Every anchor here is therefore resolved *onto the silhouette*, by
//! casting a ray from the shape's centre and taking the point where it leaves.
//!
//! Casting from the centre rather than snapping to the nearest edge is what makes
//! the eight compass anchors behave: on a five-pointed star the east anchor lands
//! on the tip of the east point, which is where a connector visibly belongs, not on
//! whichever concave valley happens to be closest.

use crate::outline::Outline;
use crate::unit::{Point, p};

/// The eight standard attachment points Miro offers on a shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AnchorKind {
    North,
    NorthEast,
    East,
    SouthEast,
    South,
    SouthWest,
    West,
    NorthWest,
}

impl AnchorKind {
    /// Clockwise from the top, matching how the handles are laid out on screen.
    pub const ALL: [Self; 8] = [
        Self::North,
        Self::NorthEast,
        Self::East,
        Self::SouthEast,
        Self::South,
        Self::SouthWest,
        Self::West,
        Self::NorthWest,
    ];

    /// The corresponding corner or edge midpoint of the unit box — the normalised
    /// point Miro would store for this anchor.
    pub const fn box_point(self) -> Point {
        match self {
            Self::North => p(0.5, 0.0),
            Self::NorthEast => p(1.0, 0.0),
            Self::East => p(1.0, 0.5),
            Self::SouthEast => p(1.0, 1.0),
            Self::South => p(0.5, 1.0),
            Self::SouthWest => p(0.0, 1.0),
            Self::West => p(0.0, 0.5),
            Self::NorthWest => p(0.0, 0.0),
        }
    }

    /// Stable token for the document format and for logs.
    pub const fn tag(self) -> &'static str {
        match self {
            Self::North => "n",
            Self::NorthEast => "ne",
            Self::East => "e",
            Self::SouthEast => "se",
            Self::South => "s",
            Self::SouthWest => "sw",
            Self::West => "w",
            Self::NorthWest => "nw",
        }
    }
}

/// A resolved attachment point: where the connector actually meets the shape.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Anchor {
    pub kind: AnchorKind,
    /// On the silhouette, in unit-box coordinates.
    pub point: Point,
}

impl Outline {
    /// Resolves an arbitrary normalised point onto the silhouette.
    ///
    /// The point is treated as a *direction* from the centre: the returned point is
    /// where a ray from the centre through `requested` leaves the shape. That keeps
    /// the two properties a connector needs — the anchor is always on the outline,
    /// and it is always on the side of the shape the user pointed at.
    ///
    /// `{0.5, 0.5}` is the centre and names no direction; it resolves to the east
    /// anchor rather than to nothing, since a connector must still land somewhere.
    /// Callers that mean "auto-route" should pick a side themselves.
    pub fn resolve_anchor(&self, requested: Point) -> Point {
        let centre = self.bounds().centre();
        let direction = requested - centre;
        let direction = if direction.length() < 1e-6 { p(1.0, 0.0) } else { direction };
        self.ray_exit(centre, direction)
            // A ray from the centre misses only when the centre is outside the
            // shape — an arc band is the case that occurs in practice. Falling back
            // to the closest point to the request keeps the anchor on the shape.
            .unwrap_or_else(|| self.nearest_point(requested))
    }

    pub fn anchor(&self, kind: AnchorKind) -> Anchor {
        Anchor { kind, point: self.resolve_anchor(kind.box_point()) }
    }

    /// All eight anchors, clockwise from the top.
    pub fn anchors(&self) -> [Anchor; 8] {
        AnchorKind::ALL.map(|kind| self.anchor(kind))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Shape;

    #[test]
    fn on_a_rectangle_anchors_are_the_box_points_themselves() {
        let rect = Shape::Rectangle.outline(1.0);
        for anchor in rect.anchors() {
            let expected = anchor.kind.box_point();
            assert!(
                anchor.point.distance(expected) < 1e-4,
                "{:?} resolved to {:?}, expected {expected:?}",
                anchor.kind,
                anchor.point
            );
        }
    }

    /// The case that motivates the whole module: on a diamond the four corner
    /// anchors are outside the shape, and must slide onto its edges.
    #[test]
    fn diamond_corner_anchors_slide_onto_the_edges() {
        let diamond = Shape::Diamond.outline(1.0);
        let ne = diamond.anchor(AnchorKind::NorthEast);
        assert!(ne.point.distance(p(0.75, 0.25)) < 1e-3, "{:?}", ne.point);
        // The edge midpoints are vertices of the diamond and stay put.
        assert!(diamond.anchor(AnchorKind::East).point.distance(p(1.0, 0.5)) < 1e-3);
        assert!(diamond.anchor(AnchorKind::North).point.distance(p(0.5, 0.0)) < 1e-3);
    }

    #[test]
    fn star_anchors_reach_the_points_not_the_valleys() {
        let star = Shape::star().outline(1.0);
        let north = star.anchor(AnchorKind::North).point;
        assert!(north.y < 0.01, "the north anchor belongs on the tip: {north:?}");
    }

    #[test]
    fn an_arbitrary_miro_anchor_resolves_onto_the_outline() {
        let triangle = Shape::triangle().outline(1.0);
        let resolved = triangle.resolve_anchor(p(0.9, 0.1));
        assert!(triangle.nearest_point(resolved).distance(resolved) < 1e-4);
        assert!(resolved.x < 0.9, "the box point is outside the triangle, so it must move");
    }

    #[test]
    fn the_centre_resolves_to_a_real_point_rather_than_nothing() {
        let rect = Shape::Rectangle.outline(1.0);
        let resolved = rect.resolve_anchor(p(0.5, 0.5));
        assert!(resolved.distance(p(1.0, 0.5)) < 1e-4, "{resolved:?}");
    }
}
