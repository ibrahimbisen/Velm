//! Orthogonal (elbow) routing with obstacle avoidance.
//!
//! The search space is an **orthogonal visibility lattice**, not a uniform grid. A
//! uniform grid has to be fine enough to squeeze between two widgets and therefore
//! large enough to be slow, and it quantises every route onto cell boundaries, so
//! connectors that ought to be collinear end up a cell apart. Instead the lattice is
//! built from the only x and y coordinates a good route can possibly turn at: each
//! obstacle's inflated edges, the two departure points, and the midpoint between
//! them. That is `O(obstacles)` coordinates per axis rather than
//! `O(board size / cell)`, the turns land exactly on the clearance line, and the
//! route is exact rather than snapped.
//!
//! A* over that lattice, with a Manhattan heuristic — admissible, because every move
//! is axis-aligned — and two extra costs:
//!
//! - a **bend penalty** per corner, because a route with one more bend reads worse
//!   than one that is somewhat longer;
//! - a **symmetry tie-break**, a weight small enough that it can only decide between
//!   routes that are otherwise equal, pulling bends towards the midpoint between the
//!   endpoints. Without it, ties are broken by lattice iteration order, and a
//!   connector's elbow lands hard against one of the two widgets for no reason a
//!   user can see.
//!
//! Each end departs along a **stub**: a short run perpendicular to the widget's
//! edge, pushed far enough to clear that widget's own inflated footprint. The stub
//! is what makes a connector look attached to an edge rather than aimed at a corner,
//! and it is exempt from the blocking test — it necessarily crosses the widget it is
//! leaving.

use crate::geometry::{EPSILON, Point, Rect, Vec2, distance_to_segment};
use crate::route::{ObstacleAvoidance, ResolvedEndpoint, RoutedPath, Router};

/// Ceiling on obstacles considered for one route.
///
/// A route that has to weave past two dozen widgets has already failed at being
/// readable, and the lattice is quadratic in this number. Obstacles are ranked by
/// distance from the direct line, so the ones that get dropped are the ones a route
/// was never going to touch.
const MAX_OBSTACLES: usize = 24;

/// Weight on the symmetry tie-break. Small enough that a route it prefers can never
/// be more than a fraction of a pixel longer than one it rejects, on any board size
/// `f64` world space supports.
const SYMMETRY_TIE_WEIGHT: f64 = 1e-6;

/// Two lattice coordinates closer than this are the same coordinate. Without
/// merging, an obstacle edge that happens to coincide with a departure point
/// produces two lattice lines a rounding error apart and a route with an invisible
/// zero-length jog in it.
const COORD_MERGE_TOLERANCE: f64 = 1e-6;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Axis {
    Horizontal,
    Vertical,
}

impl Axis {
    fn index(self) -> usize {
        match self {
            Self::Horizontal => 0,
            Self::Vertical => 1,
        }
    }
}

/// Routes an elbow connector around `obstacles`.
pub(crate) fn route(
    router: &Router,
    start: &ResolvedEndpoint,
    end: &ResolvedEndpoint,
    obstacles: &[Rect],
) -> RoutedPath {
    let (start_stub, start_dir) = departure(router, start, end.point);
    let (end_stub, end_dir) = departure(router, end, start.point);

    let blockers = relevant_obstacles(router, obstacles, start_stub, end_stub);

    let waypoints = search(
        router,
        start_stub,
        start_dir.and_then(axis_of),
        end_stub,
        end_dir.and_then(axis_of),
        &blockers,
    )
    .unwrap_or_else(|| elbow(start_stub, start_dir, end_stub, end_dir));

    // Verified on the stub-to-stub waypoints rather than on the finished path: the
    // two segments running from an anchor out to its stub cross their own widget's
    // clearance band by construction, so including them would report every
    // well-formed route as a failure.
    let avoidance = if is_clear(&waypoints, &blockers) {
        ObstacleAvoidance::Cleared
    } else {
        ObstacleAvoidance::FellBack
    };

    let mut points = Vec::with_capacity(waypoints.len() + 2);
    points.push(start.point);
    points.extend(waypoints);
    points.push(end.point);
    RoutedPath::from_points(simplify(points), avoidance)
}

/// Where a connector is free to start turning, and the direction it leaves in.
///
/// The stub is pushed past the endpoint widget's own inflated footprint, not merely
/// [`Router::stub_length`] from the anchor: an anchor sits *on* the widget's edge,
/// so a fixed offset would leave the stub inside the clearance band and every route
/// out of it blocked.
///
/// Three cases:
///
/// - **An edge anchor** leaves along its edge normal. A widget rotated off-axis has
///   a diagonal normal, so the stub is diagonal too and imposes no departure *axis*;
///   only the first lattice direction is left to the search.
/// - **A centre attachment** has no edge, so it aims at the opposite endpoint and is
///   pushed out far enough to clear its own widget. Without that it would start
///   inside its own obstacle with every route out blocked.
/// - **A free endpoint** gets no stub at all. There is no edge for the connector to
///   look attached to, so a lead-in would be a kink for no reason.
fn departure(router: &Router, endpoint: &ResolvedEndpoint, toward: Point) -> (Point, Option<Vec2>) {
    let Some(bounds) = endpoint.bounds else {
        return (endpoint.point, None);
    };
    let Some(direction) =
        endpoint.normal.or_else(|| (toward - endpoint.point).normalized())
    else {
        return (endpoint.point, None);
    };
    let clear =
        bounds.aabb().inflate(router.clearance).ray_exit_distance(endpoint.point, direction);
    (endpoint.point + direction.scaled(clear + router.stub_length), Some(direction))
}

fn axis_of(direction: Vec2) -> Option<Axis> {
    if direction.y.abs() <= EPSILON {
        Some(Axis::Horizontal)
    } else if direction.x.abs() <= EPSILON {
        Some(Axis::Vertical)
    } else {
        None
    }
}

/// Inflated obstacles near enough to matter, ranked by how close they are to the
/// direct line and capped at [`MAX_OBSTACLES`].
fn relevant_obstacles(router: &Router, obstacles: &[Rect], from: Point, to: Point) -> Vec<Rect> {
    let region = Rect::from_corners(from, to).inflate(router.detour_margin);
    let mut near: Vec<(f64, Rect)> = obstacles
        .iter()
        .map(|r| r.inflate(router.clearance))
        .filter(|r| r.intersects(region))
        .map(|r| (distance_to_segment(r.center(), from, to), r))
        .collect();
    near.sort_by(|a, b| a.0.total_cmp(&b.0));
    near.truncate(MAX_OBSTACLES);
    near.into_iter().map(|(_, r)| r).collect()
}

/// The x and y coordinates a route may turn at.
fn lattice_axis(
    from: f64,
    to: f64,
    obstacle_edges: impl Iterator<Item = (f64, f64)>,
) -> Vec<f64> {
    let mut coords = vec![from, to, (from + to) / 2.0];
    for (lo, hi) in obstacle_edges {
        coords.push(lo);
        coords.push(hi);
    }
    coords.sort_by(f64::total_cmp);
    coords.dedup_by(|a, b| (*a - *b).abs() <= COORD_MERGE_TOLERANCE);
    coords
}

fn index_of(coords: &[f64], value: f64) -> Option<usize> {
    coords.iter().position(|c| (c - value).abs() <= COORD_MERGE_TOLERANCE * 2.0)
}

/// A* over the lattice. `None` when no route clears every obstacle.
fn search(
    router: &Router,
    from: Point,
    from_axis: Option<Axis>,
    to: Point,
    to_axis: Option<Axis>,
    blockers: &[Rect],
) -> Option<Vec<Point>> {
    let xs = lattice_axis(from.x, to.x, blockers.iter().map(|r| (r.min.x, r.max.x)));
    let ys = lattice_axis(from.y, to.y, blockers.iter().map(|r| (r.min.y, r.max.y)));
    let (nx, ny) = (xs.len(), ys.len());

    let start_cell = (index_of(&xs, from.x)?, index_of(&ys, from.y)?);
    let goal_cell = (index_of(&xs, to.x)?, index_of(&ys, to.y)?);

    let state = |cell: (usize, usize), axis: Axis| (cell.1 * nx + cell.0) * 2 + axis.index();
    let cell_of = |s: usize| ((s / 2) % nx, (s / 2) / nx);
    let point_of = |cell: (usize, usize)| Point::new(xs[cell.0], ys[cell.1]);

    let mid = from.midpoint(to);
    let heuristic =
        |cell: (usize, usize)| (xs[cell.0] - to.x).abs() + (ys[cell.1] - to.y).abs();

    let mut cost = vec![f64::INFINITY; nx * ny * 2];
    let mut came_from = vec![usize::MAX; nx * ny * 2];
    let mut queue = std::collections::BinaryHeap::new();

    for axis in [Axis::Horizontal, Axis::Vertical] {
        // Arriving at the start "along" the departure axis is free; any other first
        // move is a bend and is charged as one.
        let initial = match from_axis {
            Some(a) if a != axis => router.bend_penalty,
            _ => 0.0,
        };
        let s = state(start_cell, axis);
        cost[s] = initial;
        queue.push(Candidate { estimate: initial + heuristic(start_cell), state: s });
    }

    let mut best_goal: Option<usize> = None;
    while let Some(Candidate { estimate, state: current }) = queue.pop() {
        let cell = cell_of(current);
        if estimate > cost[current] + heuristic(cell) + EPSILON {
            continue; // A stale queue entry; a cheaper route to this state was found.
        }
        if cell == goal_cell {
            best_goal = Some(current);
            break;
        }

        let axis = if current % 2 == 0 { Axis::Horizontal } else { Axis::Vertical };
        let here = point_of(cell);

        for (dx, dy, move_axis) in [
            (-1i64, 0i64, Axis::Horizontal),
            (1, 0, Axis::Horizontal),
            (0, -1, Axis::Vertical),
            (0, 1, Axis::Vertical),
        ] {
            let Some(nxt) = step(cell, dx, dy, nx, ny) else { continue };
            let there = point_of(nxt);
            if blockers.iter().any(|r| r.intersects_segment(here, there)) {
                continue;
            }

            let mut step_cost = here.distance_to(there);
            if move_axis != axis {
                step_cost += router.bend_penalty
                    + SYMMETRY_TIE_WEIGHT * ((here.x - mid.x).abs() + (here.y - mid.y).abs());
            }
            if nxt == goal_cell && matches!(to_axis, Some(a) if a != move_axis) {
                step_cost += router.bend_penalty;
            }

            let candidate = cost[current] + step_cost;
            let s = state(nxt, move_axis);
            if candidate + EPSILON < cost[s] {
                cost[s] = candidate;
                came_from[s] = current;
                queue.push(Candidate { estimate: candidate + heuristic(nxt), state: s });
            }
        }
    }

    let mut cursor = best_goal?;
    let mut points = vec![point_of(cell_of(cursor))];
    while came_from[cursor] != usize::MAX {
        cursor = came_from[cursor];
        points.push(point_of(cell_of(cursor)));
    }
    points.reverse();
    Some(points)
}

fn step(cell: (usize, usize), dx: i64, dy: i64, nx: usize, ny: usize) -> Option<(usize, usize)> {
    let x = cell.0.checked_add_signed(dx as isize)?;
    let y = cell.1.checked_add_signed(dy as isize)?;
    (x < nx && y < ny).then_some((x, y))
}

/// A* frontier entry, ordered as a min-heap on the estimated total cost.
struct Candidate {
    estimate: f64,
    state: usize,
}

impl Ord for Candidate {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // Reversed, because `BinaryHeap` is a max-heap. `total_cmp` rather than
        // `partial_cmp` so the ordering is total even if a cost ever goes non-finite.
        other
            .estimate
            .total_cmp(&self.estimate)
            .then_with(|| other.state.cmp(&self.state))
    }
}

impl PartialOrd for Candidate {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Candidate {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == std::cmp::Ordering::Equal
    }
}

impl Eq for Candidate {}

/// The route to use when no obstacle-free one exists.
///
/// Fewest bends, split at the midpoint so the result is symmetric. The one subtlety
/// is *which* midpoint: when both ends leave along the same axis and the far end is
/// **behind** the near one — two widgets facing away from each other, or overlapping
/// — splitting along the departure axis produces a route that doubles back through
/// itself. Splitting perpendicular instead gives the C-shape a person would draw.
/// This route is already the degraded case; it should at least not be self-crossing.
fn elbow(from: Point, from_dir: Option<Vec2>, to: Point, to_dir: Option<Vec2>) -> Vec<Point> {
    let chord = to - from;
    let dominant =
        if chord.x.abs() >= chord.y.abs() { Axis::Horizontal } else { Axis::Vertical };
    let a = from_dir.and_then(axis_of).unwrap_or(dominant);
    let b = to_dir.and_then(axis_of).unwrap_or(a);
    let ahead = from_dir.is_none_or(|d| chord.dot(d) > 0.0);

    match (a, b) {
        (Axis::Horizontal, Axis::Horizontal) if ahead => {
            let mx = (from.x + to.x) / 2.0;
            vec![from, Point::new(mx, from.y), Point::new(mx, to.y), to]
        }
        (Axis::Vertical, Axis::Vertical) if ahead => {
            let my = (from.y + to.y) / 2.0;
            vec![from, Point::new(from.x, my), Point::new(to.x, my), to]
        }
        (Axis::Horizontal, Axis::Horizontal) => {
            let my = (from.y + to.y) / 2.0;
            vec![from, Point::new(from.x, my), Point::new(to.x, my), to]
        }
        (Axis::Vertical, Axis::Vertical) => {
            let mx = (from.x + to.x) / 2.0;
            vec![from, Point::new(mx, from.y), Point::new(mx, to.y), to]
        }
        (Axis::Horizontal, Axis::Vertical) => vec![from, Point::new(to.x, from.y), to],
        (Axis::Vertical, Axis::Horizontal) => vec![from, Point::new(from.x, to.y), to],
    }
}

/// Drops points that add nothing: duplicates, and vertices where the route carries
/// straight on.
fn simplify(points: Vec<Point>) -> Vec<Point> {
    let mut out: Vec<Point> = Vec::with_capacity(points.len());
    for p in points {
        if out.last().is_some_and(|last| last.coincides_with(p)) {
            continue;
        }
        if out.len() >= 2 {
            let (a, b) = (out[out.len() - 2], out[out.len() - 1]);
            let (u, v) = (b - a, p - b);
            if (u.x * v.y - u.y * v.x).abs() <= EPSILON * u.length().max(1.0) && u.dot(v) > 0.0 {
                out.pop();
            }
        }
        out.push(p);
    }
    out
}

/// Whether every segment of a waypoint list stays out of every obstacle.
fn is_clear(points: &[Point], blockers: &[Rect]) -> bool {
    points
        .windows(2)
        .all(|w| !blockers.iter().any(|r| r.intersects_segment(w[0], w[1])))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::anchor::{Anchor, WidgetBounds, WidgetId};
    use crate::route::{Connector, Endpoint};
    use crate::style::{ConnectorStyle, RoutingMode};

    fn widget(x: f64, y: f64, w: f64, h: f64) -> WidgetBounds {
        WidgetBounds::new(Point::new(x, y), w, h, 0.0)
    }

    fn elbow_style() -> ConnectorStyle {
        ConnectorStyle { routing: RoutingMode::Orthogonal, ..ConnectorStyle::default() }
    }

    fn route_between(
        widgets: &[WidgetBounds],
        a: (usize, Anchor),
        b: (usize, Anchor),
        obstacles: &[Rect],
    ) -> RoutedPath {
        let connector = Connector::new(
            Endpoint::bound(WidgetId(a.0), a.1),
            Endpoint::bound(WidgetId(b.0), b.1),
            elbow_style(),
        );
        Router::default().route(&connector, &widgets.to_vec(), obstacles).unwrap()
    }

    fn all_segments_axis_aligned(path: &RoutedPath) -> bool {
        let pts = path.flatten(0.05).points;
        pts.windows(2).all(|w| {
            (w[0].x - w[1].x).abs() <= 1e-6 || (w[0].y - w[1].y).abs() <= 1e-6
        })
    }

    fn bends(path: &RoutedPath) -> usize {
        path.flatten(0.05).points.len().saturating_sub(2)
    }

    /// No pair of consecutive segments reverses direction. A route that retraces
    /// part of itself is legible as a bug even when it is geometrically valid, and
    /// it is the specific way a naive elbow fails when the two ends face away from
    /// each other.
    fn never_doubles_back(path: &RoutedPath) -> bool {
        let pts = path.flatten(0.05).points;
        pts.windows(3).all(|w| (w[1] - w[0]).dot(w[2] - w[1]) >= -1e-9)
    }

    #[test]
    fn a_clear_route_is_axis_aligned_and_symmetric() {
        let widgets = vec![widget(0.0, 0.0, 100.0, 60.0), widget(500.0, 200.0, 100.0, 60.0)];
        let path = route_between(&widgets, (0, Anchor::RIGHT), (1, Anchor::LEFT), &[]);

        assert!(all_segments_axis_aligned(&path));
        assert_eq!(path.start, Point::new(50.0, 0.0));
        assert_eq!(path.end(), Point::new(450.0, 200.0));
        assert_eq!(bends(&path), 2, "{path:?}");

        // Both ends leave horizontally, so the vertical leg belongs on the midline.
        let jog = path.flatten(0.05).points[1].x;
        assert!((jog - (50.0 + 450.0) / 2.0).abs() < 1e-6, "elbow at {jog}, not the midpoint");
    }

    #[test]
    fn opposing_departure_axes_produce_a_single_bend() {
        let widgets = vec![widget(0.0, 0.0, 100.0, 60.0), widget(500.0, 400.0, 100.0, 60.0)];
        let path = route_between(&widgets, (0, Anchor::RIGHT), (1, Anchor::TOP), &[]);
        assert!(all_segments_axis_aligned(&path));
        assert_eq!(bends(&path), 1, "{path:?}");
    }

    /// The case the whole module exists for: a rectangle sitting squarely between
    /// the two endpoints.
    #[test]
    fn a_blocking_rectangle_is_routed_around() {
        let widgets = vec![widget(0.0, 0.0, 100.0, 60.0), widget(600.0, 0.0, 100.0, 60.0)];
        let wall = Rect::from_center_size(Point::new(300.0, 0.0), 120.0, 400.0);
        let obstacles = [widgets[0].aabb(), widgets[1].aabb(), wall];

        let path = route_between(&widgets, (0, Anchor::RIGHT), (1, Anchor::LEFT), &obstacles);
        assert_eq!(path.avoidance, ObstacleAvoidance::Cleared, "{path:?}");
        assert!(all_segments_axis_aligned(&path));

        // No part of the route may enter the wall.
        let pts = path.flatten(0.05).points;
        assert!(
            pts.windows(2).all(|w| !wall.intersects_segment(w[0], w[1])),
            "route crosses the wall: {pts:?}"
        );
        // And it keeps the requested clearance.
        let padded = wall.inflate(Router::default().clearance);
        assert!(pts.iter().all(|p| !padded.inflate(-1e-6).contains(*p)), "{pts:?}");
        // The straight line would have been 500px; going around must cost more.
        assert!(path.length() > 500.0);
    }

    #[test]
    fn a_wall_with_a_gap_is_threaded_rather_than_circumvented() {
        let widgets = vec![widget(0.0, 0.0, 100.0, 60.0), widget(600.0, 0.0, 100.0, 60.0)];
        let upper = Rect::from_corners(Point::new(240.0, -1000.0), Point::new(360.0, -120.0));
        let lower = Rect::from_corners(Point::new(240.0, 120.0), Point::new(360.0, 1000.0));
        let obstacles = [widgets[0].aabb(), widgets[1].aabb(), upper, lower];

        let path = route_between(&widgets, (0, Anchor::RIGHT), (1, Anchor::LEFT), &obstacles);
        assert_eq!(path.avoidance, ObstacleAvoidance::Cleared);
        // Straight through the gap: the two anchors are already collinear.
        assert_eq!(bends(&path), 0, "{path:?}");
        assert!((path.length() - 500.0).abs() < 1e-6);
    }

    /// Widgets that overlap leave nowhere to route: the stubs land inside each
    /// other's clearance bands. That must degrade to a usable elbow and say so,
    /// not hang, panic, or return a route that claims to be clear.
    #[test]
    fn overlapping_widgets_fall_back_and_report_it() {
        let widgets = vec![widget(0.0, 0.0, 300.0, 300.0), widget(40.0, 40.0, 300.0, 300.0)];
        let obstacles = [widgets[0].aabb(), widgets[1].aabb()];
        let path = route_between(&widgets, (0, Anchor::RIGHT), (1, Anchor::LEFT), &obstacles);

        assert_eq!(path.avoidance, ObstacleAvoidance::FellBack, "{path:?}");
        assert!(path.flatten(0.05).points.iter().all(|p| p.is_finite()));
        assert!(all_segments_axis_aligned(&path));
        // Degraded, but still legible: the fallback must not retrace itself.
        assert!(never_doubles_back(&path), "{:?}", path.flatten(0.05).points);
    }

    /// Two widgets facing away from each other. Splitting along the departure axis
    /// would send the route back through its own first segment.
    #[test]
    fn widgets_facing_away_from_each_other_get_a_c_shape() {
        let widgets = vec![widget(500.0, 0.0, 100.0, 60.0), widget(0.0, 0.0, 100.0, 60.0)];
        let obstacles = [widgets[0].aabb(), widgets[1].aabb()];
        let path = route_between(&widgets, (0, Anchor::RIGHT), (1, Anchor::LEFT), &obstacles);

        assert_eq!(path.avoidance, ObstacleAvoidance::Cleared, "{path:?}");
        assert!(never_doubles_back(&path), "{:?}", path.flatten(0.05).points);
        assert!(all_segments_axis_aligned(&path));
        // It goes out, around and back, so it must leave the widgets' shared band.
        let pts = path.flatten(0.05).points;
        assert!(pts.iter().any(|p| p.y.abs() > 30.0), "{pts:?}");
    }

    /// A centre attachment has no edge normal, so it starts *inside* its own widget.
    /// It has to be pushed out toward the far end before the search can begin, or
    /// every route out of it is blocked by the widget it is attached to.
    #[test]
    fn a_centre_attachment_escapes_its_own_widget() {
        let widgets = vec![widget(0.0, 0.0, 100.0, 60.0), widget(500.0, 200.0, 100.0, 60.0)];
        let obstacles = [widgets[0].aabb(), widgets[1].aabb()];
        let path = route_between(&widgets, (0, Anchor::CENTER), (1, Anchor::CENTER), &obstacles);

        assert_eq!(path.start, Point::ORIGIN);
        assert_eq!(path.end(), Point::new(500.0, 200.0));
        assert_eq!(path.avoidance, ObstacleAvoidance::Cleared, "{path:?}");
        assert!(path.length().is_finite());
    }

    #[test]
    fn a_free_endpoint_gets_no_perpendicular_lead_in() {
        // A free end has no edge to look attached to, so the route heads for it
        // directly rather than approaching along a stub.
        let widgets = vec![widget(0.0, 0.0, 100.0, 60.0)];
        let connector = Connector::new(
            Endpoint::bound(WidgetId(0), Anchor::RIGHT),
            Endpoint::Free(Point::new(400.0, 0.0)),
            elbow_style(),
        );
        let path = Router::default()
            .route(&connector, &widgets, &[widgets[0].aabb()])
            .unwrap();
        assert_eq!(bends(&path), 0, "{path:?}");
        assert!((path.length() - 350.0).abs() < 1e-6);
    }

    #[test]
    fn widgets_touching_edge_to_edge_still_route() {
        let widgets = vec![widget(0.0, 0.0, 100.0, 100.0), widget(100.0, 0.0, 100.0, 100.0)];
        let obstacles = [widgets[0].aabb(), widgets[1].aabb()];
        let path = route_between(&widgets, (0, Anchor::RIGHT), (1, Anchor::LEFT), &obstacles);

        assert_eq!(path.start, Point::new(50.0, 0.0));
        assert_eq!(path.end(), Point::new(50.0, 0.0));
        assert!(path.is_degenerate(), "coincident anchors are a zero-length connector");
    }

    #[test]
    fn a_widget_fully_containing_another_still_produces_a_finite_route() {
        let widgets = vec![widget(0.0, 0.0, 800.0, 800.0), widget(0.0, 0.0, 100.0, 100.0)];
        let obstacles = [widgets[0].aabb(), widgets[1].aabb()];
        let path = route_between(&widgets, (1, Anchor::RIGHT), (0, Anchor::RIGHT), &obstacles);
        assert!(path.flatten(0.05).points.iter().all(|p| p.is_finite()));
        assert!(path.length().is_finite());
    }

    /// Both ends on one widget: the route has to leave, go round and come back.
    #[test]
    fn a_self_connector_leaves_and_returns() {
        let widgets = vec![widget(0.0, 0.0, 200.0, 100.0)];
        let obstacles = [widgets[0].aabb()];
        let path = route_between(&widgets, (0, Anchor::RIGHT), (0, Anchor::TOP), &obstacles);

        assert_eq!(path.start, Point::new(100.0, 0.0));
        assert_eq!(path.end(), Point::new(0.0, -50.0));
        assert_eq!(path.avoidance, ObstacleAvoidance::Cleared, "{path:?}");
        assert!(all_segments_axis_aligned(&path));
        // It must go round the corner, not cut through the widget.
        let pts = path.flatten(0.05).points;
        let body = widgets[0].aabb();
        assert!(pts[1..pts.len() - 1].windows(2).all(|w| !body.intersects_segment(w[0], w[1])));
    }

    #[test]
    fn a_rotated_widget_departs_along_its_real_edge_normal() {
        let widgets = vec![
            WidgetBounds::new(Point::ORIGIN, 200.0, 100.0, 45.0),
            widget(600.0, 0.0, 100.0, 60.0),
        ];
        let obstacles = [widgets[0].aabb(), widgets[1].aabb()];
        let path = route_between(&widgets, (0, Anchor::RIGHT), (1, Anchor::LEFT), &obstacles);

        // The first leg follows the 45° normal, so it is the one segment that is not
        // axis-aligned; everything after it is.
        let dir = path.start_tangent().unwrap();
        let k = std::f64::consts::FRAC_1_SQRT_2;
        assert!((dir.x - k).abs() < 1e-6 && (dir.y - k).abs() < 1e-6, "{dir:?}");
        let pts = path.flatten(0.05).points;
        assert!(
            pts[1..].windows(2).all(|w| (w[0].x - w[1].x).abs() < 1e-6 || (w[0].y - w[1].y).abs() < 1e-6),
            "{pts:?}"
        );
    }

    #[test]
    fn free_endpoints_route_orthogonally() {
        let start = ResolvedEndpoint::free(Point::new(0.0, 0.0));
        let end = ResolvedEndpoint::free(Point::new(300.0, 200.0));
        let path = Router::default().route_resolved(&start, &end, RoutingMode::Orthogonal, &[]);
        assert!(all_segments_axis_aligned(&path));
        assert_eq!(path.start, Point::ORIGIN);
        assert_eq!(path.end(), Point::new(300.0, 200.0));
        assert!((path.length() - 500.0).abs() < 1e-6, "Manhattan distance, {path:?}");
    }

    #[test]
    fn collinear_waypoints_are_simplified_away() {
        let simplified = simplify(vec![
            Point::new(0.0, 0.0),
            Point::new(5.0, 0.0),
            Point::new(10.0, 0.0),
            Point::new(10.0, 0.0),
            Point::new(10.0, 10.0),
        ]);
        assert_eq!(simplified, vec![Point::new(0.0, 0.0), Point::new(10.0, 0.0), Point::new(10.0, 10.0)]);
    }

    #[test]
    fn a_doubling_back_vertex_is_kept() {
        // Collinear but reversing direction: dropping the middle point would delete
        // the turn entirely.
        let simplified = simplify(vec![
            Point::new(0.0, 0.0),
            Point::new(10.0, 0.0),
            Point::new(5.0, 0.0),
        ]);
        assert_eq!(simplified.len(), 3);
    }

    #[test]
    fn many_obstacles_stay_within_the_search_budget() {
        let widgets = vec![widget(0.0, 0.0, 60.0, 60.0), widget(1200.0, 0.0, 60.0, 60.0)];
        let mut obstacles = vec![widgets[0].aabb(), widgets[1].aabb()];
        for i in 0..200 {
            let x = 100.0 + f64::from(i) * 5.0;
            obstacles.push(Rect::from_center_size(Point::new(x, 400.0), 40.0, 40.0));
        }
        let path = route_between(&widgets, (0, Anchor::RIGHT), (1, Anchor::LEFT), &obstacles);
        assert!(path.length().is_finite());
        assert!(all_segments_axis_aligned(&path));
    }
}
