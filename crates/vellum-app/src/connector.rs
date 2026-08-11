//! Connector geometry, derived from live bounds every time it is asked for.
//!
//! `vellum_doc::ConnectorEnd` stores a **binding** — an item id plus a normalised
//! attachment on that item's box — rather than a world point, and
//! `docs/02-miro-formats.md` calls preserving that indirection the point of
//! importing connectors at all. This module is where the indirection is cashed in:
//! nothing here is cached, because a connector whose geometry is stored is a
//! connector that lies the moment either end is dragged.
//!
//! Everything is resolved against the item's **unrotated placement plus its
//! rotation**, not against the axis-aligned box the spatial index holds. A connector
//! attached to the right edge of a widget turned 30° has to meet that edge, not the
//! right side of its bounding box.

use vellum_connect::{
    Anchor, ConnectorStyle, Point, ResolvedEndpoint, RoutedPath, Router, RoutingMode, WidgetBounds,
};
use vellum_doc::{ArrowKind, ConnectorEnd, Dash, ItemId, ItemKind, Placement, Routing};

/// Turns a document placement into the rectangle a connector anchors onto.
pub fn widget_bounds(placement: &Placement) -> WidgetBounds {
    let (width, height) = placement.scaled_size();
    WidgetBounds::new(
        Point::new(placement.x, placement.y),
        width,
        height,
        placement.rotation,
    )
}

/// Resolves one end of a connector.
///
/// `own` is the connector item's own placement, which is what an *unbound* end
/// attaches to — that is the rule `ConnectorEnd::anchor` documents, and it is what
/// gives a free end real bounds for culling instead of a second coordinate space.
pub fn resolve_end(
    end: &ConnectorEnd,
    own: &Placement,
    target: Option<&Placement>,
) -> ResolvedEndpoint {
    let anchor = Anchor::new(end.anchor.0, end.anchor.1);
    // A binding whose target is gone falls back to the connector's own rectangle
    // rather than to the origin: a dangling connector should stay where it was
    // drawn, which is also where the user will look for it to repair it.
    let bounds = widget_bounds(target.unwrap_or(own));
    ResolvedEndpoint {
        point: bounds.resolve(anchor),
        normal: bounds.outward_normal(anchor),
        bounds: Some(bounds),
    }
}

/// The attachments a **drawn** connector may use: the four edge midpoints, in the order
/// ties are broken.
///
/// Miro has only ever been observed emitting these four plus the centre —
/// `ConnectorEnd::anchor`'s own note records that — and this set deliberately leaves the
/// centre out. `ConnectorEnd::CENTER` is a real attachment and imported connectors use it,
/// but a *drawn* one should not: a straight line between two centres is drawn through both
/// shapes, so dragging from the middle of one box to the middle of another — which is how
/// anybody draws a connector — would produce a line that starts and ends inside the things
/// it connects. Edge midpoints give the picture the user was aiming at.
pub const ANCHORS: [(f64, f64); 4] = [
    ConnectorEnd::TOP,
    ConnectorEnd::RIGHT,
    ConnectorEnd::BOTTOM,
    ConnectorEnd::LEFT,
];

/// Which of [`ANCHORS`] on `placement` **faces** a world point.
///
/// `toward` is the connector's *other* endpoint, not the point the pointer was released
/// at. That is the whole trick: a release lands inside the shape roughly always, so
/// measuring to the release point would pick whichever edge the pointer happened to be
/// nearest inside the box, and two boxes side by side would be joined top-to-top as often
/// as right-to-left. Measuring to the far end picks the pair of edges that face each
/// other, which is what Miro draws and what the diagram means.
///
/// Measured against the **rotation-correct** resolved position of each anchor, not against
/// the axis-aligned box: on a widget turned 30° the nearest edge midpoint by bounding box
/// is frequently not the nearest one on screen.
///
/// Ties go to the earlier entry in [`ANCHORS`], so a point exactly off a corner picks the
/// top edge over the right one. Arbitrary, but stable and therefore testable, which
/// matters more than which of two equally good answers it is.
pub fn facing_anchor(placement: &Placement, toward: (f64, f64)) -> (f64, f64) {
    let bounds = widget_bounds(placement);
    ANCHORS
        .into_iter()
        .min_by(|a, b| {
            let distance = |anchor: (f64, f64)| {
                let at = bounds.resolve(Anchor::new(anchor.0, anchor.1));
                (at.x - toward.0).hypot(at.y - toward.1)
            };
            distance(*a).total_cmp(&distance(*b))
        })
        // `ANCHORS` is a non-empty array literal, so `min_by` cannot fail; the default
        // keeps this total rather than adding an `expect` to a geometry helper.
        .unwrap_or(ConnectorEnd::CENTER)
}

/// The world position of one of a placement's anchors.
pub fn anchor_point(placement: &Placement, anchor: (f64, f64)) -> (f64, f64) {
    let at = widget_bounds(placement).resolve(Anchor::new(anchor.0, anchor.1));
    (at.x, at.y)
}

/// The smallest box a connector between two world points can own, so its free ends have
/// somewhere to be normalised against.
///
/// **Not zero-width even for a vertical line.** A free end is stored as a fraction of the
/// connector's own rectangle — `ConnectorEnd::anchor` says so — so a zero extent in either
/// axis makes that fraction a division by zero, and every point on the line collapses onto
/// one coordinate. `MIN_EXTENT` is small enough to be invisible and large enough to divide
/// by.
pub const MIN_EXTENT: f64 = 1.0;

/// The placement a new connector owns, given its two endpoints in world space.
pub fn placement_for(from: (f64, f64), to: (f64, f64)) -> Placement {
    let (left, right) = (from.0.min(to.0), from.0.max(to.0));
    let (top, bottom) = (from.1.min(to.1), from.1.max(to.1));
    let width = (right - left).max(MIN_EXTENT);
    let height = (bottom - top).max(MIN_EXTENT);
    Placement::new((left + right) / 2.0, (top + bottom) / 2.0, width, height)
}

/// A world point as a normalised anchor on `own` — how a **free** end is stored.
///
/// The inverse of the rule in `ConnectorEnd::anchor`: a free end's fraction is of the
/// connector's own rectangle. Rotation is not undone because a connector placed by hand is
/// never rotated at the moment it is created; if one is rotated afterwards its free ends
/// travel with it, which is the same behaviour every other unbound geometry has.
pub fn free_anchor(own: &Placement, world: (f64, f64)) -> (f64, f64) {
    let (width, height) = own.scaled_size();
    let (left, top) = (own.x - width / 2.0, own.y - height / 2.0);
    (
        ((world.0 - left) / width.max(MIN_EXTENT)).clamp(0.0, 1.0),
        ((world.1 - top) / height.max(MIN_EXTENT)).clamp(0.0, 1.0),
    )
}

/// Everything needed to draw one connector.
pub struct Routed {
    pub path: RoutedPath,
    pub style: ConnectorStyle,
}

/// Routes a connector item. Returns `None` for anything that is not one.
///
/// `placement_of` looks up a bound target's placement; returning `None` from it is
/// how a deleted target is reported, and is handled rather than propagated.
pub fn route(
    item_kind: &ItemKind,
    own: &Placement,
    placement_of: impl Fn(ItemId) -> Option<Placement>,
    router: &Router,
    obstacles: &[vellum_connect::Rect],
) -> Option<Routed> {
    let ItemKind::Connector { start, end, routing, dash, thickness, .. } = item_kind else {
        return None;
    };

    let target = |e: &ConnectorEnd| e.target.and_then(&placement_of);
    let start_placement = target(start);
    let end_placement = target(end);

    let from = resolve_end(start, own, start_placement.as_ref());
    let to = resolve_end(end, own, end_placement.as_ref());

    let style = ConnectorStyle {
        routing: routing_mode(*routing),
        line: line_style(*dash),
        // A connector scales with its item, like everything else placed.
        thickness: (thickness * own.scale).max(0.0),
        start_arrow: arrowhead(start.arrowhead),
        end_arrow: arrowhead(end.arrowhead),
        // Jump-overs are a Miro key the document does not carry yet; see the
        // module's gap note in `docs/features/README.md` §1.
        jump_overs: false,
    };

    Some(Routed {
        path: router.route_resolved(&from, &to, style.routing, obstacles),
        style,
    })
}

/// The document's routing mode, in the geometry crate's spelling. The two enums are
/// named one for one precisely so this stays a rename rather than a lookup table.
pub const fn routing_mode(routing: Routing) -> RoutingMode {
    match routing {
        Routing::Straight => RoutingMode::Straight,
        Routing::Curved => RoutingMode::Curved,
        Routing::Orthogonal => RoutingMode::Orthogonal,
    }
}

pub const fn line_style(dash: Dash) -> vellum_connect::LineStyle {
    match dash {
        Dash::Solid => vellum_connect::LineStyle::Solid,
        Dash::Dashed => vellum_connect::LineStyle::Dashed,
        Dash::Dotted => vellum_connect::LineStyle::Dotted,
    }
}

pub const fn arrowhead(kind: ArrowKind) -> vellum_connect::Arrowhead {
    match kind {
        ArrowKind::None => vellum_connect::Arrowhead::None,
        ArrowKind::LineArrow => vellum_connect::Arrowhead::LineArrow,
        ArrowKind::FilledTriangle => vellum_connect::Arrowhead::FilledTriangle,
        ArrowKind::OpenTriangle => vellum_connect::Arrowhead::OpenTriangle,
        ArrowKind::Circle => vellum_connect::Arrowhead::Circle,
        ArrowKind::FilledCircle => vellum_connect::Arrowhead::FilledCircle,
        ArrowKind::Diamond => vellum_connect::Arrowhead::Diamond,
        ArrowKind::FilledDiamond => vellum_connect::Arrowhead::FilledDiamond,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vellum_doc::StyledText;

    fn connector(start: ConnectorEnd, end: ConnectorEnd) -> ItemKind {
        ItemKind::Connector {
            start,
            end,
            routing: Routing::Straight,
            dash: Dash::Solid,
            thickness: 2.0,
            color: None,
            captions: Vec::new(),
        }
    }

    /// The property the whole indirection exists for: move the target, and the
    /// connector's endpoint moves with it without anything being rewritten.
    #[test]
    fn an_endpoint_follows_the_item_it_is_bound_to() {
        let target = "1@1".parse::<ItemId>().unwrap();
        let own = Placement::new(0.0, 0.0, 10.0, 10.0);
        let kind = connector(
            ConnectorEnd::bound(target, ConnectorEnd::RIGHT),
            ConnectorEnd::free(ConnectorEnd::CENTER),
        );

        let at = |x: f64| {
            let placement = Placement::new(x, 0.0, 200.0, 100.0);
            route(&kind, &own, |_| Some(placement), &Router::default(), &[])
                .expect("a connector routes")
                .path
                .start
        };

        assert_eq!(at(0.0), Point::new(100.0, 0.0));
        assert_eq!(at(500.0), Point::new(600.0, 0.0));
    }

    /// A rotated widget's right edge is not the right side of its bounding box, and
    /// getting that wrong is invisible until someone turns a shape.
    #[test]
    fn an_anchor_resolves_onto_a_rotated_edge() {
        let target = "1@1".parse::<ItemId>().unwrap();
        let mut placement = Placement::new(0.0, 0.0, 200.0, 100.0);
        placement.rotation = 90.0;

        let end = ConnectorEnd::bound(target, ConnectorEnd::RIGHT);
        let resolved = resolve_end(&end, &Placement::default(), Some(&placement));

        // Turned a quarter turn clockwise, the right edge's midpoint is below the
        // centre rather than to its right.
        assert!(resolved.point.x.abs() < 1e-9, "{:?}", resolved.point);
        assert!((resolved.point.y - 100.0).abs() < 1e-9, "{:?}", resolved.point);
    }

    /// A deleted target must leave the connector where it was drawn rather than
    /// collapsing it onto the world origin.
    #[test]
    fn a_dangling_binding_falls_back_to_the_connectors_own_rectangle() {
        let missing = "9@9".parse::<ItemId>().unwrap();
        let own = Placement::new(400.0, 300.0, 200.0, 100.0);
        let kind = connector(
            ConnectorEnd::bound(missing, ConnectorEnd::LEFT),
            ConnectorEnd::bound(missing, ConnectorEnd::RIGHT),
        );

        let routed = route(&kind, &own, |_| None, &Router::default(), &[]).unwrap();
        assert_eq!(routed.path.start, Point::new(300.0, 300.0));
        assert_eq!(routed.path.end(), Point::new(500.0, 300.0));
    }

    #[test]
    fn thickness_follows_the_items_scale() {
        let mut own = Placement::new(0.0, 0.0, 100.0, 100.0);
        own.scale = 3.0;
        let kind = connector(
            ConnectorEnd::free(ConnectorEnd::LEFT),
            ConnectorEnd::free(ConnectorEnd::RIGHT),
        );
        let routed = route(&kind, &own, |_| None, &Router::default(), &[]).unwrap();
        assert_eq!(routed.style.thickness, 6.0);
    }

    #[test]
    fn styles_map_across_without_a_lookup_table() {
        assert_eq!(routing_mode(Routing::Orthogonal), RoutingMode::Orthogonal);
        assert_eq!(line_style(Dash::Dotted), vellum_connect::LineStyle::Dotted);
        assert_eq!(
            arrowhead(ArrowKind::FilledTriangle),
            vellum_connect::Arrowhead::FilledTriangle
        );
        assert_eq!(arrowhead(ArrowKind::None), vellum_connect::Arrowhead::None);
    }

    /// Anchors are picked against the **rotated** position. On a widget turned 90° the
    /// nearest edge midpoint by bounding box is routinely not the nearest one on screen.
    #[test]
    fn the_facing_anchor_is_measured_after_rotation() {
        let square = Placement::new(0.0, 0.0, 100.0, 100.0);
        assert_eq!(facing_anchor(&square, (200.0, 0.0)), ConnectorEnd::RIGHT);
        assert_eq!(facing_anchor(&square, (0.0, -200.0)), ConnectorEnd::TOP);

        // Turned 90° clockwise, the anchor that now *faces* right is the one that was on
        // top. Picking by bounding box would still answer RIGHT.
        let turned = Placement { rotation: 90.0, ..square };
        assert_eq!(facing_anchor(&turned, (200.0, 0.0)), ConnectorEnd::TOP);
    }

    /// The reason the anchor is measured to the *far* endpoint rather than to where the
    /// pointer was released: a release lands inside the shape roughly always, so two boxes
    /// side by side must still be joined right-to-left.
    #[test]
    fn two_boxes_side_by_side_are_joined_by_their_facing_edges() {
        let left = Placement::new(0.0, 0.0, 100.0, 100.0);
        let right = Placement::new(400.0, 0.0, 100.0, 100.0);
        assert_eq!(facing_anchor(&left, (right.x, right.y)), ConnectorEnd::RIGHT);
        assert_eq!(facing_anchor(&right, (left.x, left.y)), ConnectorEnd::LEFT);

        // And one above the other, top-to-bottom.
        let below = Placement::new(0.0, 400.0, 100.0, 100.0);
        assert_eq!(facing_anchor(&left, (below.x, below.y)), ConnectorEnd::BOTTOM);
        assert_eq!(facing_anchor(&below, (left.x, left.y)), ConnectorEnd::TOP);
    }

    /// The centre is not in the candidate set at all. A straight line between two centres
    /// is drawn *through* both shapes, which is not the picture anybody dragging from the
    /// middle of one box to the middle of another is asking for.
    #[test]
    fn a_drawn_connector_never_attaches_to_a_centre() {
        assert!(!ANCHORS.contains(&ConnectorEnd::CENTER));
        let square = Placement::new(0.0, 0.0, 100.0, 100.0);
        // Even asked from its own centre, where the centre attachment would be nearest.
        assert_ne!(facing_anchor(&square, (0.0, 0.0)), ConnectorEnd::CENTER);
    }

    /// The trap a free end sets: its anchor is a *fraction* of the connector's own box, so
    /// a perfectly vertical or horizontal line would divide by zero without a floor.
    #[test]
    fn a_straight_line_still_has_a_box_to_normalise_against() {
        let vertical = placement_for((10.0, 0.0), (10.0, 200.0));
        assert!(vertical.width >= MIN_EXTENT, "width {}", vertical.width);
        assert!((vertical.height - 200.0).abs() < 1e-9);

        // And the two ends round-trip to the fractions that put them back.
        let from = free_anchor(&vertical, (10.0, 0.0));
        let to = free_anchor(&vertical, (10.0, 200.0));
        assert!((from.1 - 0.0).abs() < 1e-9, "{from:?}");
        assert!((to.1 - 1.0).abs() < 1e-9, "{to:?}");
        assert!(from.0.is_finite() && to.0.is_finite(), "a zero extent divided by zero");

        let degenerate = placement_for((5.0, 5.0), (5.0, 5.0));
        assert!(degenerate.width >= MIN_EXTENT && degenerate.height >= MIN_EXTENT);
        assert!(free_anchor(&degenerate, (5.0, 5.0)).0.is_finite());
    }

    /// A free end drawn where the pointer was released ends up drawn there — the property
    /// that makes a hand-placed connector land on its own preview.
    #[test]
    fn a_free_end_resolves_back_to_the_point_it_was_drawn_at() {
        let (from, to) = ((-40.0, 20.0), (160.0, 90.0));
        let own = placement_for(from, to);
        let start = ConnectorEnd::free(free_anchor(&own, from));
        let end = ConnectorEnd::free(free_anchor(&own, to));

        let resolved = resolve_end(&start, &own, None).point;
        assert!((resolved.x - from.0).abs() < 1e-9 && (resolved.y - from.1).abs() < 1e-9);
        let resolved = resolve_end(&end, &own, None).point;
        assert!((resolved.x - to.0).abs() < 1e-9 && (resolved.y - to.1).abs() < 1e-9);
    }

    #[test]
    fn anything_that_is_not_a_connector_routes_to_nothing() {
        let sticky = ItemKind::Sticky { text: StyledText::plain("x"), background: None };
        assert!(
            route(&sticky, &Placement::default(), |_| None, &Router::default(), &[]).is_none()
        );
    }
}
