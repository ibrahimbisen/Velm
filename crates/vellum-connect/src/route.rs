//! Turning a pair of endpoints into a path.
//!
//! A [`Connector`] holds *bindings*, not coordinates. Routing resolves those
//! bindings against live widget bounds every time it runs, which is what makes a
//! connector re-route when either end is dragged. The output, a [`RoutedPath`], is
//! the only thing downstream code measures, hit-tests or tessellates.

use serde::{Deserialize, Serialize};

use crate::anchor::{Anchor, BoundsSource, WidgetBounds, WidgetId};
use crate::geometry::{EPSILON, Point, Polyline, Rect, Vec2};
use crate::orthogonal;
use crate::style::{ConnectorStyle, RoutingMode};

/// Default flattening tolerance, in world px: the maximum distance between a curve
/// and the polyline standing in for it.
///
/// 0.05px is below a device pixel at 1× zoom and stays below one until roughly 20×,
/// past which the flattening would need redoing anyway.
pub const DEFAULT_TOLERANCE: f64 = 0.05;

/// Everything that can go wrong turning a connector into geometry.
#[derive(Debug, thiserror::Error)]
pub enum ConnectError {
    /// A bound endpoint names a widget the [`BoundsSource`] does not know.
    ///
    /// Reported rather than silently dropped: on a Miro import a `widgetIndex` that
    /// resolves to nothing means the payload and the widget array have diverged,
    /// and a connector that quietly vanishes is exactly the kind of partial import
    /// `docs/02-miro-formats.md` insists on over-reporting.
    #[error("connector endpoint binds to widget {0}, which has no bounds")]
    UnknownTarget(WidgetId),
    #[error("tessellation failed: {0}")]
    Tessellation(#[from] lyon::tessellation::TessellationError),
}

/// One end of a connector: bound to a widget, or floating free.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum Endpoint {
    /// Attached to a widget at a normalised [`Anchor`]. Re-resolved on every route,
    /// so the connector follows the widget.
    Bound { target: WidgetId, anchor: Anchor },
    /// Pinned to a fixed world point — Miro's `widgetIndex: null` case, and the
    /// state a connector is in while the user is dragging one end onto nothing.
    Free(Point),
}

impl Endpoint {
    pub const fn bound(target: WidgetId, anchor: Anchor) -> Self {
        Self::Bound { target, anchor }
    }

    pub fn target(self) -> Option<WidgetId> {
        match self {
            Self::Bound { target, .. } => Some(target),
            Self::Free(_) => None,
        }
    }
}

/// A connector: two endpoints and the style that decides how they are joined.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Connector {
    pub start: Endpoint,
    pub end: Endpoint,
    pub style: ConnectorStyle,
}

impl Connector {
    pub fn new(start: Endpoint, end: Endpoint, style: ConnectorStyle) -> Self {
        Self { start, end, style }
    }
}

/// An endpoint after its binding has been looked up: where it is, and what the
/// widget's edge implies about the direction the connector should leave in.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ResolvedEndpoint {
    pub point: Point,
    /// The outward edge normal, when the endpoint is bound to an edge. `None` for a
    /// free endpoint and for a centre attachment, neither of which has an edge.
    pub normal: Option<Vec2>,
    /// The widget the endpoint sits on, if any. The router needs it to push the
    /// departure stub clear of the widget's own footprint.
    pub bounds: Option<WidgetBounds>,
}

impl ResolvedEndpoint {
    /// A free endpoint at a world point.
    pub fn free(point: Point) -> Self {
        Self { point, normal: None, bounds: None }
    }
}

/// One piece of a routed path.
///
/// Two variants, not three: jump-over arcs are emitted as pairs of cubic
/// quarter-circles rather than as a dedicated arc segment. A cubic approximates a
/// quarter circle to about 0.02% of its radius, which for a 6px hop is four orders
/// of magnitude below a device pixel, and keeping the vocabulary small means every
/// consumer — flattening, bounds, hit-testing, tessellation — has two cases instead
/// of three.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum PathSegment {
    Line { to: Point },
    Cubic { control1: Point, control2: Point, to: Point },
}

impl PathSegment {
    pub fn to(self) -> Point {
        match self {
            Self::Line { to } | Self::Cubic { to, .. } => to,
        }
    }
}

/// Whether obstacle avoidance was attempted, and whether it worked.
///
/// Exposed rather than kept internal because "the route may cross a widget" is
/// something the UI can act on — by nudging, by warning, or by leaving it alone —
/// and a router that silently degrades is indistinguishable from one that is broken.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ObstacleAvoidance {
    /// The routing mode does not route around anything.
    #[default]
    NotAttempted,
    /// A route was found that clears every obstacle considered.
    Cleared,
    /// No clear route existed. A plain elbow was used and it may cross an obstacle.
    FellBack,
}

/// A connector's geometry, in world space.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoutedPath {
    pub start: Point,
    pub segments: Vec<PathSegment>,
    pub avoidance: ObstacleAvoidance,
}

impl RoutedPath {
    /// A path through a sequence of world points. Consecutive duplicates are
    /// dropped; an empty input produces an empty path rather than panicking.
    pub fn from_points(points: impl IntoIterator<Item = Point>, avoidance: ObstacleAvoidance) -> Self {
        let line = Polyline::new(points);
        let start = line.points.first().copied().unwrap_or(Point::ORIGIN);
        Self {
            start,
            segments: line.points.iter().skip(1).map(|&to| PathSegment::Line { to }).collect(),
            avoidance,
        }
    }

    /// A degenerate path standing for a connector whose two ends coincide.
    ///
    /// It keeps a single zero-length segment rather than none, so that [`Self::end`]
    /// and [`Self::flatten`] behave the same way they do for every other path and
    /// callers need no special case.
    pub fn degenerate(at: Point) -> Self {
        Self {
            start: at,
            segments: vec![PathSegment::Line { to: at }],
            avoidance: ObstacleAvoidance::NotAttempted,
        }
    }

    pub fn end(&self) -> Point {
        self.segments.last().map_or(self.start, |s| s.to())
    }

    /// True when the path has no extent — both endpoints on the same anchor of the
    /// same widget, or a free endpoint dropped exactly on a bound one.
    pub fn is_degenerate(&self) -> bool {
        self.flatten(DEFAULT_TOLERANCE).length() <= EPSILON
    }

    /// The polyline standing in for the path at a given flattening tolerance.
    pub fn flatten(&self, tolerance: f64) -> Polyline {
        let mut points = vec![self.start];
        let mut cursor = self.start;
        for segment in &self.segments {
            match *segment {
                PathSegment::Line { to } => points.push(to),
                PathSegment::Cubic { control1, control2, to } => {
                    cubic(cursor, control1, control2, to).for_each_flattened(
                        tolerance.max(EPSILON),
                        &mut |line| points.push(Point::new(line.to.x, line.to.y)),
                    );
                }
            }
            cursor = segment.to();
        }
        Polyline::new(points)
    }

    /// Arc length along the routed path.
    pub fn length(&self) -> f64 {
        self.flatten(DEFAULT_TOLERANCE).length()
    }

    /// The tight bounding box of the path's centreline.
    ///
    /// Exact, not the convex hull of the control points: cubic segments are bounded
    /// by solving for the extrema of the curve itself. A hull-based box would report
    /// a jump-over arc as up to a third taller than it is, which shows up as
    /// unnecessary redraws and as clicks landing outside the item.
    pub fn bounds(&self) -> Rect {
        let mut rect = Rect::from_corners(self.start, self.start);
        let mut cursor = self.start;
        for segment in &self.segments {
            match *segment {
                PathSegment::Line { to } => rect = rect.union_point(to),
                PathSegment::Cubic { control1, control2, to } => {
                    let bb = cubic(cursor, control1, control2, to).bounding_box();
                    rect = rect.union(Rect::from_corners(
                        Point::new(bb.min.x, bb.min.y),
                        Point::new(bb.max.x, bb.max.y),
                    ));
                }
            }
            cursor = segment.to();
        }
        rect
    }

    /// The bounding box of the drawn stroke.
    ///
    /// With the round joins and caps this crate tessellates with, the stroke is
    /// exactly the centreline's Minkowski sum with a disc of radius `thickness / 2`,
    /// so inflating the tight centreline box by that radius is exact rather than
    /// conservative. Arrowheads are not included — they are generated at
    /// tessellation time from the style, which bounds does not see.
    pub fn stroke_bounds(&self, thickness: f64) -> Rect {
        self.bounds().inflate(thickness.max(0.0) / 2.0)
    }

    /// Distance from a point to the path's centreline.
    pub fn distance_to(&self, point: Point) -> f64 {
        self.flatten(DEFAULT_TOLERANCE).distance_to(point)
    }

    /// Whether a point is within `tolerance` of the centreline.
    ///
    /// `tolerance` is measured from the centreline, so a caller that wants "did the
    /// user click the visible line" passes `half the thickness plus a grab margin`.
    /// Folding thickness in here instead would make the picking margin invisible to
    /// the UI, and a 1px connector needs a much bigger grab margin than an 8px one.
    pub fn hit_test(&self, point: Point, tolerance: f64) -> bool {
        self.distance_to(point) <= tolerance.max(0.0)
    }

    /// Unit direction the path sets off in, or `None` when it has no extent.
    pub fn start_tangent(&self) -> Option<Vec2> {
        let first = *self.segments.first()?;
        match first {
            PathSegment::Line { to } => (to - self.start).normalized(),
            PathSegment::Cubic { control1, control2, to } => (control1 - self.start)
                .normalized()
                .or_else(|| (control2 - self.start).normalized())
                .or_else(|| (to - self.start).normalized()),
        }
    }

    /// Unit direction the path arrives in, or `None` when it has no extent.
    pub fn end_tangent(&self) -> Option<Vec2> {
        let last = *self.segments.last()?;
        let previous = if self.segments.len() >= 2 {
            self.segments[self.segments.len() - 2].to()
        } else {
            self.start
        };
        match last {
            PathSegment::Line { to } => (to - previous).normalized(),
            PathSegment::Cubic { control1, control2, to } => (to - control2)
                .normalized()
                .or_else(|| (to - control1).normalized())
                .or_else(|| (to - previous).normalized()),
        }
    }
}

/// Tunable routing behaviour.
///
/// Every field is a length in world px except [`Router::curve_strength`], so the
/// defaults describe a board at 1× zoom. They are deliberately independent of
/// connector thickness: routing decides where a connector goes, and a thicker line
/// should not take a different path.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Router {
    /// How far an orthogonal route stays clear of an obstacle.
    pub clearance: f64,
    /// How far a route runs perpendicular to a widget's edge before it is free to
    /// turn. Without it, a connector leaves at whatever angle the next waypoint
    /// happens to be in and reads as if it is attached to the corner.
    pub stub_length: f64,
    /// Extra cost charged for each corner in an orthogonal route, in px-equivalent.
    /// Large relative to typical widget spacing, because a route with one more bend
    /// looks worse than one that is a little longer.
    pub bend_penalty: f64,
    /// Fraction of the endpoint separation a curved connector's control points are
    /// projected out along the edge normal.
    pub curve_strength: f64,
    /// Ceiling on that projection, so a connector spanning a whole board does not
    /// bulge halfway across it.
    pub max_control_offset: f64,
    /// How far outside the endpoints' bounding box an obstacle can be and still be
    /// considered. Bounds both the search and the length of the detour a route is
    /// willing to make.
    pub detour_margin: f64,
}

impl Default for Router {
    fn default() -> Self {
        Self {
            clearance: 12.0,
            stub_length: 24.0,
            bend_penalty: 40.0,
            curve_strength: 0.45,
            max_control_offset: 240.0,
            detour_margin: 400.0,
        }
    }
}

impl Router {
    /// Looks up an endpoint's binding and derives its departure direction.
    pub fn resolve_endpoint<B: BoundsSource + ?Sized>(
        &self,
        endpoint: Endpoint,
        source: &B,
    ) -> Result<ResolvedEndpoint, ConnectError> {
        match endpoint {
            Endpoint::Free(point) => Ok(ResolvedEndpoint::free(point)),
            Endpoint::Bound { target, anchor } => {
                let bounds = source.bounds(target).ok_or(ConnectError::UnknownTarget(target))?;
                Ok(ResolvedEndpoint {
                    point: bounds.resolve(anchor),
                    normal: bounds.outward_normal(anchor),
                    bounds: Some(bounds),
                })
            }
        }
    }

    /// Routes a connector, resolving both endpoints first.
    ///
    /// `obstacles` are axis-aligned rectangles the orthogonal mode routes around;
    /// straight and curved ignore them, matching Miro, where only elbow connectors
    /// avoid anything. A rotated widget contributes its
    /// [`WidgetBounds::aabb`](crate::WidgetBounds::aabb) — which over-reserves the
    /// corners, so a route may keep more distance from a turned widget than it
    /// strictly needs. That is the safe direction to be wrong in, and it keeps the
    /// blocking test to an interval overlap per axis.
    pub fn route<B: BoundsSource + ?Sized>(
        &self,
        connector: &Connector,
        source: &B,
        obstacles: &[Rect],
    ) -> Result<RoutedPath, ConnectError> {
        let start = self.resolve_endpoint(connector.start, source)?;
        let end = self.resolve_endpoint(connector.end, source)?;
        Ok(self.route_resolved(&start, &end, connector.style.routing, obstacles))
    }

    /// Routes between two already-resolved endpoints.
    ///
    /// Separate from [`Self::route`] so that an interactive drag — where one end is
    /// following the cursor and has no binding to look up — takes the same code
    /// path as an imported connector.
    pub fn route_resolved(
        &self,
        start: &ResolvedEndpoint,
        end: &ResolvedEndpoint,
        mode: RoutingMode,
        obstacles: &[Rect],
    ) -> RoutedPath {
        if start.point.coincides_with(end.point) {
            return RoutedPath::degenerate(start.point);
        }
        match mode {
            RoutingMode::Straight => RoutedPath {
                start: start.point,
                segments: vec![PathSegment::Line { to: end.point }],
                avoidance: ObstacleAvoidance::NotAttempted,
            },
            RoutingMode::Curved => self.route_curved(start, end),
            RoutingMode::Orthogonal => orthogonal::route(self, start, end, obstacles),
        }
    }

    /// A cubic whose control points are projected out along each endpoint's edge
    /// normal, so the curve leaves and enters perpendicular to the shapes rather
    /// than at whatever angle the chord happens to be.
    ///
    /// An endpoint with no edge — free, or attached at a widget's centre — falls
    /// back to the chord direction, which degrades to a gentle S rather than to a
    /// kink.
    fn route_curved(&self, start: &ResolvedEndpoint, end: &ResolvedEndpoint) -> RoutedPath {
        let chord = end.point - start.point;
        let span = chord.length();
        let offset = (span * self.curve_strength).min(self.max_control_offset);

        let forward = chord.normalized().unwrap_or(Vec2::X);
        let out_of_start = start.normal.unwrap_or(forward);
        let out_of_end = end.normal.unwrap_or(-forward);

        RoutedPath {
            start: start.point,
            segments: vec![PathSegment::Cubic {
                control1: start.point + out_of_start.scaled(offset),
                control2: end.point + out_of_end.scaled(offset),
                to: end.point,
            }],
            avoidance: ObstacleAvoidance::NotAttempted,
        }
    }
}

/// Builds the lyon cubic used for flattening and for exact bounds.
///
/// `f64` throughout: world coordinates reach tens of thousands of px, and rebasing
/// to a local origin happens once, at tessellation, rather than being smeared
/// through every geometric query.
fn cubic(from: Point, c1: Point, c2: Point, to: Point) -> lyon::geom::CubicBezierSegment<f64> {
    use lyon::geom::euclid::default::Point2D;
    lyon::geom::CubicBezierSegment {
        from: Point2D::new(from.x, from.y),
        ctrl1: Point2D::new(c1.x, c1.y),
        ctrl2: Point2D::new(c2.x, c2.y),
        to: Point2D::new(to.x, to.y),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn widget(x: f64, y: f64, w: f64, h: f64) -> WidgetBounds {
        WidgetBounds::new(Point::new(x, y), w, h, 0.0)
    }

    fn style(routing: RoutingMode) -> ConnectorStyle {
        ConnectorStyle { routing, ..ConnectorStyle::default() }
    }

    #[test]
    fn a_straight_route_joins_the_two_resolved_anchors() {
        let widgets = vec![widget(0.0, 0.0, 100.0, 50.0), widget(400.0, 0.0, 100.0, 50.0)];
        let connector = Connector::new(
            Endpoint::bound(WidgetId(0), Anchor::RIGHT),
            Endpoint::bound(WidgetId(1), Anchor::LEFT),
            style(RoutingMode::Straight),
        );
        let path = Router::default().route(&connector, &widgets, &[]).unwrap();

        assert_eq!(path.start, Point::new(50.0, 0.0));
        assert_eq!(path.end(), Point::new(350.0, 0.0));
        assert_eq!(path.segments.len(), 1);
        assert!((path.length() - 300.0).abs() < 1e-9);
    }

    #[test]
    fn a_missing_target_is_reported_not_swallowed() {
        let widgets: Vec<WidgetBounds> = vec![widget(0.0, 0.0, 10.0, 10.0)];
        let connector = Connector::new(
            Endpoint::bound(WidgetId(0), Anchor::RIGHT),
            Endpoint::bound(WidgetId(7), Anchor::LEFT),
            style(RoutingMode::Straight),
        );
        let err = Router::default().route(&connector, &widgets, &[]).unwrap_err();
        assert!(matches!(err, ConnectError::UnknownTarget(WidgetId(7))), "{err:?}");
        assert!(err.to_string().contains("#7"));
    }

    #[test]
    fn every_combination_of_bound_and_free_endpoints_routes() {
        let widgets = vec![widget(0.0, 0.0, 100.0, 50.0), widget(400.0, 0.0, 100.0, 50.0)];
        let bound_a = Endpoint::bound(WidgetId(0), Anchor::RIGHT);
        let bound_b = Endpoint::bound(WidgetId(1), Anchor::LEFT);
        let free = Endpoint::Free(Point::new(200.0, 200.0));

        for mode in [RoutingMode::Straight, RoutingMode::Curved, RoutingMode::Orthogonal] {
            for (a, b) in [(bound_a, bound_b), (bound_a, free), (free, bound_b), (free, free)] {
                let c = Connector::new(a, b, style(mode));
                let path = Router::default().route(&c, &widgets, &[]).unwrap();
                assert!(path.length() > 0.0 || a == b, "{mode:?} {a:?} -> {b:?}");
                assert!(path.start.is_finite() && path.end().is_finite());
            }
        }
    }

    #[test]
    fn a_curved_route_leaves_perpendicular_to_each_edge() {
        let widgets = vec![widget(0.0, 0.0, 100.0, 50.0), widget(400.0, 300.0, 100.0, 50.0)];
        let connector = Connector::new(
            Endpoint::bound(WidgetId(0), Anchor::RIGHT),
            Endpoint::bound(WidgetId(1), Anchor::TOP),
            style(RoutingMode::Curved),
        );
        let path = Router::default().route(&connector, &widgets, &[]).unwrap();

        // Leaves the right edge heading right, arrives at the top edge heading down.
        let start_dir = path.start_tangent().unwrap();
        assert!((start_dir.x - 1.0).abs() < 1e-9 && start_dir.y.abs() < 1e-9, "{start_dir:?}");
        let end_dir = path.end_tangent().unwrap();
        assert!(end_dir.y > 0.9 && end_dir.x.abs() < 1e-9, "{end_dir:?}");
    }

    #[test]
    fn a_curved_route_with_free_ends_falls_back_to_the_chord() {
        let start = ResolvedEndpoint::free(Point::ORIGIN);
        let end = ResolvedEndpoint::free(Point::new(100.0, 0.0));
        let path = Router::default().route_resolved(&start, &end, RoutingMode::Curved, &[]);
        let dir = path.start_tangent().unwrap();
        assert!((dir.x - 1.0).abs() < 1e-9 && dir.y.abs() < 1e-9, "{dir:?}");
        // Degenerating to the chord means the curve stays on the chord.
        assert!(path.bounds().height() < 1e-9);
    }

    /// Both ends on one widget. Miro allows it, and the two anchors resolve to
    /// different points, so nothing about it is degenerate — but a router that
    /// assumes two distinct widgets divides by a zero-length separation somewhere.
    #[test]
    fn both_endpoints_on_the_same_widget() {
        let widgets = vec![widget(0.0, 0.0, 200.0, 100.0)];
        for mode in [RoutingMode::Straight, RoutingMode::Curved, RoutingMode::Orthogonal] {
            let c = Connector::new(
                Endpoint::bound(WidgetId(0), Anchor::LEFT),
                Endpoint::bound(WidgetId(0), Anchor::RIGHT),
                style(mode),
            );
            let path = Router::default()
                .route(&c, &widgets, &[widgets[0].aabb()])
                .unwrap();
            assert_eq!(path.start, Point::new(-100.0, 0.0));
            assert_eq!(path.end(), Point::new(100.0, 0.0));
            assert!(path.length() >= 200.0, "{mode:?} produced {path:?}");
            assert!(path.flatten(DEFAULT_TOLERANCE).points.iter().all(|p| p.is_finite()));
        }
    }

    /// The same anchor twice: zero length, and every accessor must still answer.
    #[test]
    fn a_zero_length_connector_is_handled_everywhere() {
        let widgets = vec![widget(10.0, 20.0, 100.0, 50.0)];
        for mode in [RoutingMode::Straight, RoutingMode::Curved, RoutingMode::Orthogonal] {
            let c = Connector::new(
                Endpoint::bound(WidgetId(0), Anchor::RIGHT),
                Endpoint::bound(WidgetId(0), Anchor::RIGHT),
                style(mode),
            );
            let path = Router::default().route(&c, &widgets, &[]).unwrap();
            let at = Point::new(60.0, 20.0);

            assert!(path.is_degenerate(), "{mode:?}");
            assert_eq!(path.start, at);
            assert_eq!(path.end(), at);
            assert!((path.length() - 0.0).abs() < 1e-9);
            assert_eq!(path.bounds(), Rect::from_corners(at, at));
            assert!(path.hit_test(at, 0.5));
            assert!(!path.hit_test(Point::new(100.0, 20.0), 0.5));
            assert_eq!(path.start_tangent(), None);
            assert_eq!(path.end_tangent(), None);
            assert_eq!(path.flatten(DEFAULT_TOLERANCE).len(), 1);
        }
    }

    #[test]
    fn a_free_to_free_zero_length_connector_is_handled() {
        let at = Point::new(-3.0, 9.0);
        let c = Connector::new(Endpoint::Free(at), Endpoint::Free(at), style(RoutingMode::Curved));
        let widgets: Vec<WidgetBounds> = Vec::new();
        let path = Router::default().route(&c, &widgets, &[]).unwrap();
        assert!(path.is_degenerate());
        assert_eq!(path.bounds().center(), at);
    }

    #[test]
    fn hit_testing_follows_the_routed_path_not_the_chord() {
        let path = RoutedPath::from_points(
            [
                Point::new(0.0, 0.0),
                Point::new(100.0, 0.0),
                Point::new(100.0, 100.0),
            ],
            ObstacleAvoidance::Cleared,
        );
        assert!(path.hit_test(Point::new(100.0, 50.0), 0.5));
        assert!(path.hit_test(Point::new(50.0, 0.4), 0.5));
        // On the straight chord between the endpoints, but nowhere near the route.
        assert!(!path.hit_test(Point::new(50.0, 50.0), 0.5));
    }

    #[test]
    fn stroke_bounds_grow_by_exactly_half_the_thickness() {
        let path = RoutedPath::from_points(
            [Point::new(0.0, 0.0), Point::new(100.0, 40.0)],
            ObstacleAvoidance::NotAttempted,
        );
        let tight = path.bounds();
        let fat = path.stroke_bounds(8.0);
        assert!((fat.min.x - (tight.min.x - 4.0)).abs() < 1e-9);
        assert!((fat.max.y - (tight.max.y + 4.0)).abs() < 1e-9);
    }

    /// A cubic's tight bounds are strictly inside its control hull; reporting the
    /// hull would over-report the height of every curved connector.
    #[test]
    fn cubic_bounds_are_tight_not_the_control_hull() {
        let path = RoutedPath {
            start: Point::new(0.0, 0.0),
            segments: vec![PathSegment::Cubic {
                control1: Point::new(0.0, 100.0),
                control2: Point::new(100.0, 100.0),
                to: Point::new(100.0, 0.0),
            }],
            avoidance: ObstacleAvoidance::NotAttempted,
        };
        let b = path.bounds();
        assert!((b.min.y).abs() < 1e-9);
        // The curve only reaches 3/4 of the way to its control points.
        assert!((b.max.y - 75.0).abs() < 1e-6, "{b:?}");
    }

    #[test]
    fn from_points_collapses_repeats_and_survives_emptiness() {
        let p = RoutedPath::from_points([], ObstacleAvoidance::NotAttempted);
        assert_eq!(p.start, Point::ORIGIN);
        assert!(p.segments.is_empty());
        assert_eq!(p.end(), Point::ORIGIN);

        let q = RoutedPath::from_points(
            [Point::new(1.0, 1.0), Point::new(1.0, 1.0), Point::new(2.0, 1.0)],
            ObstacleAvoidance::NotAttempted,
        );
        assert_eq!(q.segments.len(), 1);
    }
}
