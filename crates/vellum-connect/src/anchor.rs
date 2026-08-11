//! Binding a connector endpoint to a widget.
//!
//! Miro stores a connector end as `{"point": {"x": 1, "y": 0.5}, "widgetIndex": 219}`
//! — a **normalised 0–1 attachment on the target's bounds**, not a world coordinate.
//! Preserving that indirection is the entire value of importing connectors: an
//! anchor re-resolves every time the widget moves, rotates or is resized, so the
//! connector follows. Baking it down to a world point at import time produces a dead
//! line that detaches the first time anything is dragged.
//!
//! ## Rotation is the part that goes silently wrong
//!
//! `{x: 1, y: 0.5}` names "the middle of the right edge" *in the widget's own
//! frame*. On a widget rotated 45° that edge is no longer on the right, and the
//! world point moves accordingly. Resolving the anchor against the axis-aligned
//! bounding box instead — the obvious shortcut, since that is what a spatial index
//! stores — puts the endpoint somewhere plausible but wrong, and the error grows
//! with the widget's aspect ratio. [`WidgetBounds::resolve`] rotates the local
//! offset; [`WidgetBounds::outward_normal`] rotates the edge normal with it.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::geometry::{EPSILON, Point, Rect, Vec2};

/// A widget, identified the way Miro identifies one: by its index in the clipboard
/// payload's `objects` array.
///
/// `docs/02-miro-formats.md` establishes that Miro references objects positionally
/// and that `id == index` for every object observed, so an index is the identity an
/// importer actually has to work with. The newtype exists so an index into some
/// other array cannot be passed by mistake.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct WidgetId(pub usize);

impl std::fmt::Display for WidgetId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "#{}", self.0)
    }
}

/// An attachment point on a widget's bounds, normalised to `0..=1` in the widget's
/// own unrotated frame: `(0, 0)` is the top-left corner, `(1, 1)` the bottom-right,
/// `(0.5, 0.5)` the centre.
///
/// Values are *not* clamped. Miro has only ever been observed emitting the four edge
/// midpoints and the centre, but an out-of-range value is preserved rather than
/// silently pulled onto the boundary, so a future Miro change shows up as an
/// endpoint in the wrong place — which is investigable — instead of one quietly
/// snapped to an edge.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Anchor {
    pub x: f64,
    pub y: f64,
}

impl Anchor {
    /// The four edge midpoints, which are the only anchors on the reference board's
    /// 18 connectors, plus the centre.
    pub const LEFT: Self = Self { x: 0.0, y: 0.5 };
    pub const RIGHT: Self = Self { x: 1.0, y: 0.5 };
    pub const TOP: Self = Self { x: 0.5, y: 0.0 };
    pub const BOTTOM: Self = Self { x: 0.5, y: 1.0 };
    pub const CENTER: Self = Self { x: 0.5, y: 0.5 };

    pub const fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }
}

impl Anchor {
    /// Which named point this is, or [`AnchorSide::Custom`] for any other fraction.
    ///
    /// Compared with a tolerance rather than for equality: an anchor can arrive from
    /// Miro's JSON as `0.9999999999999999`, and a picker that showed *Custom* for what is
    /// visibly the right edge would be reporting a difference the user cannot see and
    /// cannot act on. [`EPSILON`] is the crate's own geometric tolerance, so this agrees
    /// with the rest of the routing arithmetic.
    pub fn side(self) -> AnchorSide {
        for (candidate, side) in [
            (Self::TOP, AnchorSide::Top),
            (Self::RIGHT, AnchorSide::Right),
            (Self::BOTTOM, AnchorSide::Bottom),
            (Self::LEFT, AnchorSide::Left),
            (Self::CENTER, AnchorSide::Centre),
        ] {
            if (self.x - candidate.x).abs() < EPSILON && (self.y - candidate.y).abs() < EPSILON {
                return side;
            }
        }
        AnchorSide::Custom
    }
}

impl Default for Anchor {
    fn default() -> Self {
        Self::CENTER
    }
}

/// One of the five points a connector end can be tied to, named.
///
/// [`Anchor`] is a pair of fractions, which is the right thing to *store* and the wrong
/// thing to offer in a picker: "0.5, 0" is the top edge and nobody wants to type it. This
/// is that vocabulary, plus the two states a picker has to be able to **report** without
/// offering — an anchor that is some other fraction, and an end that is not attached to an
/// item at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AnchorSide {
    Top,
    Right,
    Bottom,
    Left,
    /// The middle of the item. Reachable from a picker and deliberately *not* by drawing —
    /// see this module's `ANCHORS` note in `vellum-app`: a straight line between two
    /// centres is drawn through both shapes.
    Centre,
    /// Some other fraction of the box. Miro's own boards contain these, so it is reported
    /// rather than rounded to the nearest edge.
    Custom,
    /// The end is pinned to the canvas rather than to an item, so it has no side. Its
    /// anchor is a fraction of the *connector's* own rectangle, which is why choosing a
    /// side for it would move the endpoint somewhere nobody asked for.
    Free,
}

impl AnchorSide {
    /// The five that can be chosen, in the order a picker should list them.
    pub const PICKABLE: [Self; 5] =
        [Self::Top, Self::Right, Self::Bottom, Self::Left, Self::Centre];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Top => "Top",
            Self::Right => "Right",
            Self::Bottom => "Bottom",
            Self::Left => "Left",
            Self::Centre => "Centre",
            Self::Custom => "Custom",
            Self::Free => "Unattached",
        }
    }

    /// The fractions to store, or `None` for the two states that name no point.
    pub const fn anchor(self) -> Option<Anchor> {
        match self {
            Self::Top => Some(Anchor::TOP),
            Self::Right => Some(Anchor::RIGHT),
            Self::Bottom => Some(Anchor::BOTTOM),
            Self::Left => Some(Anchor::LEFT),
            Self::Centre => Some(Anchor::CENTER),
            Self::Custom | Self::Free => None,
        }
    }

    /// Whether a picker may put an end here.
    pub const fn is_pickable(self) -> bool {
        self.anchor().is_some()
    }
}

/// A widget's oriented bounds: where it is, how big it is *as rendered*, and how far
/// it is turned.
///
/// `width`/`height` are the rendered size. Miro keeps size and scale in separate
/// fields — the reference board's stickies are `199 × 228` at `scale: 1.85` — and
/// resolving an anchor against the unscaled size shortens every connector by the
/// scale factor. [`WidgetBounds::from_miro`] does the multiplication so the mistake
/// is not available to make.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct WidgetBounds {
    /// World-space centre. Miro's positions denote centres, so this needs no
    /// conversion at the import boundary.
    pub center: Point,
    pub width: f64,
    pub height: f64,
    /// Degrees, clockwise on screen — Miro's `rotation.rotation`.
    pub rotation_degrees: f64,
}

impl WidgetBounds {
    pub const fn new(center: Point, width: f64, height: f64, rotation_degrees: f64) -> Self {
        Self { center, width, height, rotation_degrees }
    }

    /// Builds bounds from Miro's separately-stored `size` and `scale`.
    pub fn from_miro(
        center: Point,
        width: f64,
        height: f64,
        scale: f64,
        rotation_degrees: f64,
    ) -> Self {
        Self::new(center, width * scale, height * scale, rotation_degrees)
    }

    /// The world point an anchor names.
    ///
    /// The offset is computed in the widget's own frame and *then* rotated, which is
    /// what makes `{x: 1, y: 0.5}` follow the right edge around as the widget turns.
    pub fn resolve(&self, anchor: Anchor) -> Point {
        let local = Vec2::new((anchor.x - 0.5) * self.width, (anchor.y - 0.5) * self.height);
        self.center + local.rotated_degrees(self.rotation_degrees)
    }

    /// The unit outward normal of the edge the anchor sits on, or `None` for an
    /// anchor strictly inside the bounds.
    ///
    /// This is what lets a curved connector leave a shape perpendicular to its edge
    /// and an orthogonal one depart along the right axis. A corner anchor gets the
    /// 45° bisector of its two edges, which is the only choice that treats both
    /// edges equally. An interior anchor — Miro's centre attachment — genuinely has
    /// no edge, so callers fall back to aiming at the opposite endpoint.
    pub fn outward_normal(&self, anchor: Anchor) -> Option<Vec2> {
        let (u, v) = (anchor.x - 0.5, anchor.y - 0.5);
        let on_side = (u.abs() - 0.5).abs() <= EPSILON;
        let on_cap = (v.abs() - 0.5).abs() <= EPSILON;
        let local = match (on_side, on_cap) {
            (true, true) => Vec2::new(u.signum(), v.signum()).normalized()?,
            (true, false) => Vec2::new(u.signum(), 0.0),
            (false, true) => Vec2::new(0.0, v.signum()),
            (false, false) => return None,
        };
        Some(local.rotated_degrees(self.rotation_degrees))
    }

    /// The four corners in world space, clockwise from the top-left of the widget's
    /// own frame.
    pub fn corners(&self) -> [Point; 4] {
        [
            Anchor::new(0.0, 0.0),
            Anchor::new(1.0, 0.0),
            Anchor::new(1.0, 1.0),
            Anchor::new(0.0, 1.0),
        ]
        .map(|a| self.resolve(a))
    }

    /// The axis-aligned bounding box, which for a rotated widget is strictly larger
    /// than the widget. Obstacle avoidance works in this space; see
    /// [`crate::Router::route`] for why that is an approximation and when it costs
    /// anything.
    pub fn aabb(&self) -> Rect {
        Rect::from_points(self.corners()).expect("four corners is never empty")
    }
}

/// Where the router looks up the widget a connector end is bound to.
///
/// A trait rather than a concrete map because the caller already owns this data —
/// the importer has an `objects` array, the live document has a CRDT — and copying
/// every widget's bounds into a side table on every drag is exactly the per-frame
/// cost `docs/01-architecture.md` exists to avoid.
pub trait BoundsSource {
    fn bounds(&self, id: WidgetId) -> Option<WidgetBounds>;
}

/// Positional lookup, matching Miro's own `widgetIndex` semantics.
impl BoundsSource for [WidgetBounds] {
    fn bounds(&self, id: WidgetId) -> Option<WidgetBounds> {
        self.get(id.0).copied()
    }
}

/// Positional lookup over an array that has gaps — the honest shape of a Miro
/// payload, where a `widgetIndex` may land on a connector or a group, neither of
/// which has bounds.
impl BoundsSource for [Option<WidgetBounds>] {
    fn bounds(&self, id: WidgetId) -> Option<WidgetBounds> {
        self.get(id.0).copied().flatten()
    }
}

impl BoundsSource for Vec<WidgetBounds> {
    fn bounds(&self, id: WidgetId) -> Option<WidgetBounds> {
        self.as_slice().bounds(id)
    }
}

impl BoundsSource for Vec<Option<WidgetBounds>> {
    fn bounds(&self, id: WidgetId) -> Option<WidgetBounds> {
        self.as_slice().bounds(id)
    }
}

impl BoundsSource for HashMap<WidgetId, WidgetBounds> {
    fn bounds(&self, id: WidgetId) -> Option<WidgetBounds> {
        self.get(&id).copied()
    }
}

impl<T: BoundsSource + ?Sized> BoundsSource for &T {
    fn bounds(&self, id: WidgetId) -> Option<WidgetBounds> {
        (**self).bounds(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOL: f64 = 1e-9;

    fn near(a: Point, b: Point) -> bool {
        a.distance_to(b) < 1e-6
    }

    /// A 100 × 50 widget at the origin: the right-edge midpoint is 50 to the right.
    #[test]
    fn unrotated_anchors_land_on_the_expected_edges() {
        let w = WidgetBounds::new(Point::ORIGIN, 100.0, 50.0, 0.0);
        assert!(near(w.resolve(Anchor::RIGHT), Point::new(50.0, 0.0)));
        assert!(near(w.resolve(Anchor::LEFT), Point::new(-50.0, 0.0)));
        assert!(near(w.resolve(Anchor::TOP), Point::new(0.0, -25.0)));
        assert!(near(w.resolve(Anchor::BOTTOM), Point::new(0.0, 25.0)));
        assert!(near(w.resolve(Anchor::CENTER), Point::ORIGIN));
    }

    /// The headline case. `{x: 1, y: 0.5}` on a widget rotated 45° is *not* the same
    /// world point as on an unrotated one, and it is not the AABB's right edge
    /// either — resolving against the bounding box would give (±53.03, 0).
    #[test]
    fn rotating_a_widget_moves_its_right_edge_anchor() {
        let flat = WidgetBounds::new(Point::ORIGIN, 100.0, 50.0, 0.0);
        let turned = WidgetBounds::new(Point::ORIGIN, 100.0, 50.0, 45.0);

        let unrotated = flat.resolve(Anchor::RIGHT);
        let rotated = turned.resolve(Anchor::RIGHT);

        let k = 50.0 * std::f64::consts::FRAC_1_SQRT_2;
        assert!(near(rotated, Point::new(k, k)), "{rotated:?}");
        assert!(
            unrotated.distance_to(rotated) > 1.0,
            "rotation must move the anchor, got {unrotated:?} vs {rotated:?}"
        );

        // And it is nowhere near what the axis-aligned bounding box would suggest.
        let aabb_right = Point::new(turned.aabb().max.x, 0.0);
        assert!(
            aabb_right.distance_to(rotated) > 10.0,
            "AABB resolution must not accidentally agree: {aabb_right:?} vs {rotated:?}"
        );
    }

    #[test]
    fn quarter_turns_permute_the_edges() {
        let w = WidgetBounds::new(Point::ORIGIN, 100.0, 50.0, 90.0);
        // Clockwise: the right edge ends up at the bottom, 50 down.
        assert!(near(w.resolve(Anchor::RIGHT), Point::new(0.0, 50.0)));
        // The top edge ends up on the right, 25 across.
        assert!(near(w.resolve(Anchor::TOP), Point::new(25.0, 0.0)));

        let half = WidgetBounds::new(Point::ORIGIN, 100.0, 50.0, 180.0);
        assert!(near(half.resolve(Anchor::RIGHT), Point::new(-50.0, 0.0)));
    }

    #[test]
    fn rotation_is_about_the_widget_centre_not_the_origin() {
        let w = WidgetBounds::new(Point::new(1000.0, -400.0), 100.0, 50.0, 90.0);
        assert!(near(w.resolve(Anchor::CENTER), Point::new(1000.0, -400.0)));
        assert!(near(w.resolve(Anchor::RIGHT), Point::new(1000.0, -350.0)));
    }

    #[test]
    fn a_full_turn_is_the_identity() {
        let w = WidgetBounds::new(Point::new(3.0, 7.0), 100.0, 50.0, 360.0);
        assert!(near(w.resolve(Anchor::RIGHT), Point::new(53.0, 7.0)));
    }

    #[test]
    fn negative_rotation_turns_the_other_way() {
        let w = WidgetBounds::new(Point::ORIGIN, 100.0, 50.0, -90.0);
        assert!(near(w.resolve(Anchor::RIGHT), Point::new(0.0, -50.0)));
    }

    #[test]
    fn edge_normals_rotate_with_the_widget() {
        let w = WidgetBounds::new(Point::ORIGIN, 100.0, 50.0, 45.0);
        let n = w.outward_normal(Anchor::RIGHT).unwrap();
        let k = std::f64::consts::FRAC_1_SQRT_2;
        assert!((n.x - k).abs() < TOL && (n.y - k).abs() < TOL, "{n:?}");
        assert!((n.length() - 1.0).abs() < TOL);
    }

    #[test]
    fn edge_normals_ignore_aspect_ratio() {
        // A very wide widget's right-edge normal is still exactly +x.
        let w = WidgetBounds::new(Point::ORIGIN, 4000.0, 10.0, 0.0);
        assert_eq!(w.outward_normal(Anchor::RIGHT), Some(Vec2::X));
        assert_eq!(w.outward_normal(Anchor::BOTTOM), Some(Vec2::Y));
    }

    #[test]
    fn a_corner_anchor_gets_the_bisector() {
        let w = WidgetBounds::new(Point::ORIGIN, 100.0, 50.0, 0.0);
        let n = w.outward_normal(Anchor::new(1.0, 1.0)).unwrap();
        let k = std::f64::consts::FRAC_1_SQRT_2;
        assert!((n.x - k).abs() < TOL && (n.y - k).abs() < TOL, "{n:?}");
    }

    #[test]
    fn an_interior_anchor_has_no_edge_normal() {
        let w = WidgetBounds::new(Point::ORIGIN, 100.0, 50.0, 30.0);
        assert_eq!(w.outward_normal(Anchor::CENTER), None);
        assert_eq!(w.outward_normal(Anchor::new(0.25, 0.75)), None);
    }

    #[test]
    fn a_rotated_aabb_is_larger_than_the_widget() {
        let w = WidgetBounds::new(Point::ORIGIN, 100.0, 100.0, 45.0);
        let bb = w.aabb();
        let diagonal = 100.0 * std::f64::consts::SQRT_2;
        assert!((bb.width() - diagonal).abs() < 1e-6, "{bb:?}");
        assert!((bb.height() - diagonal).abs() < 1e-6);
    }

    /// The SVG oracle, cross-checked end to end.
    ///
    /// Objects 219 and 220 of the reference board's clipboard payload are `199 × 228`
    /// stickies at `scale: 1.85`, centred at x = 6748.319 and x = 7484.619 with the
    /// same y. Connector 221 binds `{x: 1, y: 0.5}` on the first to `{x: 0, y: 0.5}`
    /// on the second. Miro's own SVG export draws that connector inside a group
    /// declared `width="368.15px"` — so if anchor resolution is right, the two
    /// resolved points must be exactly 368.15px apart. They are, which also confirms
    /// that `scale` has to be folded into the size.
    #[test]
    fn anchors_reproduce_the_connector_length_in_miros_svg_export() {
        let first = WidgetBounds::from_miro(
            Point::new(6748.319280161753, -4696.375103507173),
            199.0,
            228.0,
            1.85,
            0.0,
        );
        let second = WidgetBounds::from_miro(
            Point::new(7484.619280161751, -4696.375103507173),
            199.0,
            228.0,
            1.85,
            0.0,
        );

        let start = first.resolve(Anchor::RIGHT);
        let end = second.resolve(Anchor::LEFT);

        assert!((start.distance_to(end) - 368.15).abs() < 1e-9, "{start:?} -> {end:?}");
        // Horizontal, as the SVG's `M 0 0 L 357.97 0` requires.
        assert!((start.y - end.y).abs() < 1e-9);
    }

    /// The picker's vocabulary has to round-trip, and the two reported-only states have
    /// to stay unpickable — writing `Custom` back would mean "move the anchor to nowhere".
    #[test]
    fn every_named_side_round_trips_and_only_five_are_pickable() {
        for side in AnchorSide::PICKABLE {
            let anchor = side.anchor().expect("a pickable side names a point");
            assert_eq!(anchor.side(), side, "{side:?} did not survive the round trip");
            assert!(side.is_pickable());
        }
        assert_eq!(AnchorSide::PICKABLE.len(), 5, "four edges and the centre");
        for reported in [AnchorSide::Custom, AnchorSide::Free] {
            assert!(!reported.is_pickable());
            assert_eq!(reported.anchor(), None);
            assert!(!AnchorSide::PICKABLE.contains(&reported));
        }
    }

    /// Miro's JSON carries fractions like `0.9999999999999999`, so an exact comparison
    /// would report *Custom* for what is visibly the right edge — a difference the user
    /// can neither see nor act on. A corner is genuinely custom and must still say so.
    #[test]
    fn a_side_is_recognised_through_floating_point_noise() {
        assert_eq!(Anchor::new(0.9999999999999999, 0.5).side(), AnchorSide::Right);
        assert_eq!(Anchor::new(0.5, 0.0).side(), AnchorSide::Top);
        assert_eq!(Anchor::new(0.5, 0.5).side(), AnchorSide::Centre);
        assert_eq!(Anchor::new(0.0, 0.0).side(), AnchorSide::Custom, "a corner is not an edge");
        assert_eq!(Anchor::new(0.25, 0.5).side(), AnchorSide::Custom);
    }

    #[test]
    fn bounds_sources_resolve_by_index_and_by_key() {
        let w = WidgetBounds::new(Point::ORIGIN, 10.0, 10.0, 0.0);
        let slice = vec![w, w];
        assert_eq!(slice.bounds(WidgetId(1)), Some(w));
        assert_eq!(slice.bounds(WidgetId(2)), None);

        let sparse = vec![None, Some(w)];
        assert_eq!(sparse.bounds(WidgetId(0)), None);
        assert_eq!(sparse.bounds(WidgetId(1)), Some(w));

        let map: HashMap<_, _> = [(WidgetId(219), w)].into_iter().collect();
        assert_eq!(map.bounds(WidgetId(219)), Some(w));
        assert_eq!(map.bounds(WidgetId(0)), None);
    }
}
