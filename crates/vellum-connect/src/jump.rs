//! Jump-overs: where two connectors cross, one hops over the other.
//!
//! Miro's `jump` style key. On a dense diagram, two crossing connectors are
//! ambiguous — the eye cannot tell a crossing from a junction — and a small hop
//! resolves it. The reference board has `jump: 0` throughout, so nothing here is
//! oracle-verified; the behaviour is modelled on what the key evidently means.
//!
//! ## Which connector hops
//!
//! Somebody has to, and it must be the same one every time the board is drawn or
//! the hops flicker between the two as the draw order changes. The rule here is
//! positional: [`apply_jump_overs`] makes each connector hop over the ones **before**
//! it in the slice. Callers that want a different rule call [`insert_jumps`]
//! directly and choose their own.
//!
//! ## Hops go on straight segments only
//!
//! Crossings are *detected* against anything — the other connector is flattened, so
//! a straight connector hops over a curved one perfectly well. But the hop is only
//! *inserted* into straight segments. Splitting a cubic at a crossing means finding
//! the two parameters an arc-length window either side of it corresponds to, and a
//! cubic has no closed-form arc length; the numerics are not worth it for a case
//! that barely occurs, since curved connectors are the ones least in need of
//! disambiguation. A crossing on a curve is left as a plain crossing.

use crate::geometry::{EPSILON, Point, Polyline, segment_intersection};
use crate::route::{DEFAULT_TOLERANCE, PathSegment, RoutedPath};

/// Control-point ratio that turns a cubic into a quarter circle, to within about
/// 0.02% of the radius. Two of them make the semicircular hop.
const QUARTER_CIRCLE_CONTROL: f64 = 0.552_284_749_830_793_4;

/// How a jump-over is shaped.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct JumpOptions {
    /// Half the width of the hop, and its height. A hop wants to be clearly bigger
    /// than the lines it separates but small enough not to read as a feature of the
    /// diagram.
    pub radius: f64,
    /// Flattening tolerance used when looking for crossings.
    pub tolerance: f64,
}

impl Default for JumpOptions {
    fn default() -> Self {
        Self { radius: 6.0, tolerance: DEFAULT_TOLERANCE }
    }
}

/// Every point at which two routed paths properly cross.
///
/// Endpoint contact and collinear overlap are not crossings — see
/// [`segment_intersection`]. That matters here specifically: two connectors meeting
/// at a shared widget touch at their anchors, and hopping there would put an arc on
/// top of the widget they are both attached to.
pub fn crossings(path: &RoutedPath, other: &RoutedPath, tolerance: f64) -> Vec<Point> {
    let a = path.flatten(tolerance);
    let b = other.flatten(tolerance);
    let mut hits = Vec::new();
    for pair in a.points.windows(2) {
        for against in b.points.windows(2) {
            if let Some(p) = segment_intersection(pair[0], pair[1], against[0], against[1]) {
                hits.push(p);
            }
        }
    }
    hits
}

/// Rebuilds `path` with a hop at every point it crosses one of `others`.
pub fn insert_jumps(path: &RoutedPath, others: &[RoutedPath], options: &JumpOptions) -> RoutedPath {
    let radius = options.radius;
    if radius <= EPSILON || others.is_empty() {
        return path.clone();
    }
    let obstacles: Vec<Polyline> =
        others.iter().map(|p| p.flatten(options.tolerance)).collect();

    let mut segments = Vec::with_capacity(path.segments.len());
    let mut cursor = path.start;
    for segment in &path.segments {
        match *segment {
            PathSegment::Line { to } => {
                let mut from = cursor;
                for at in hop_positions(cursor, to, &obstacles, radius) {
                    let (entry, exit, arc) = hop(cursor, to, at, radius);
                    if !from.coincides_with(entry) {
                        segments.push(PathSegment::Line { to: entry });
                    }
                    segments.extend(arc);
                    from = exit;
                }
                if !from.coincides_with(to) {
                    segments.push(PathSegment::Line { to });
                }
            }
            other => segments.push(other),
        }
        cursor = segment.to();
    }

    RoutedPath { start: path.start, segments, avoidance: path.avoidance }
}

/// Applies the positional hop convention across a whole set of connectors.
///
/// Crossings are computed against the paths as they were *before* any hop was
/// inserted, so a connector never hops over an arc that only exists because of it.
pub fn apply_jump_overs(paths: &mut [RoutedPath], options: &JumpOptions) {
    let originals = paths.to_vec();
    for (i, path) in paths.iter_mut().enumerate().skip(1) {
        *path = insert_jumps(&originals[i], &originals[..i], options);
    }
}

/// Arc-length positions along the segment `from`–`to` that deserve a hop.
///
/// Crossings within `radius` of either end are skipped: there is no room for the
/// arc, and a hop that overruns its segment would corner into the next one. Two
/// crossings closer than a hop's width are merged for the same reason — a single
/// hop over both is what a person would draw.
fn hop_positions(from: Point, to: Point, obstacles: &[Polyline], radius: f64) -> Vec<f64> {
    let span = from.distance_to(to);
    if span <= radius * 2.0 {
        return Vec::new();
    }
    let mut positions: Vec<f64> = obstacles
        .iter()
        .flat_map(|line| line.points.windows(2))
        .filter_map(|w| segment_intersection(from, to, w[0], w[1]))
        .map(|p| from.distance_to(p))
        .filter(|d| *d >= radius && span - *d >= radius)
        .collect();
    positions.sort_by(f64::total_cmp);

    let mut kept: Vec<f64> = Vec::new();
    for d in positions {
        if kept.last().is_none_or(|last| d - last >= radius * 2.0) {
            kept.push(d);
        }
    }
    kept
}

/// The hop itself: where the line leaves, where it rejoins, and the two cubics that
/// carry it over.
///
/// The arc always bulges to the **left** of travel, so every hop on a board leans
/// the same way instead of flipping with the direction the connector happens to run.
fn hop(from: Point, to: Point, at: f64, radius: f64) -> (Point, Point, [PathSegment; 2]) {
    let forward = (to - from).normalized().unwrap_or(crate::geometry::Vec2::X);
    let left = forward.left_normal();
    let center = from + forward.scaled(at);

    let entry = center - forward.scaled(radius);
    let exit = center + forward.scaled(radius);
    let apex = center + left.scaled(radius);
    let pull = radius * QUARTER_CIRCLE_CONTROL;

    (
        entry,
        exit,
        [
            PathSegment::Cubic {
                control1: entry + left.scaled(pull),
                control2: apex - forward.scaled(pull),
                to: apex,
            },
            PathSegment::Cubic {
                control1: apex + forward.scaled(pull),
                control2: exit + left.scaled(pull),
                to: exit,
            },
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::route::ObstacleAvoidance;

    fn line(x0: f64, y0: f64, x1: f64, y1: f64) -> RoutedPath {
        RoutedPath::from_points(
            [Point::new(x0, y0), Point::new(x1, y1)],
            ObstacleAvoidance::NotAttempted,
        )
    }

    fn cubic_count(path: &RoutedPath) -> usize {
        path.segments.iter().filter(|s| matches!(s, PathSegment::Cubic { .. })).count()
    }

    #[test]
    fn crossing_connectors_are_detected() {
        let hits = crossings(&line(-50.0, 0.0, 50.0, 0.0), &line(0.0, -50.0, 0.0, 50.0), 0.05);
        assert_eq!(hits.len(), 1);
        assert!(hits[0].distance_to(Point::ORIGIN) < 1e-9);
    }

    #[test]
    fn connectors_meeting_at_a_shared_anchor_do_not_cross() {
        let hits = crossings(&line(0.0, 0.0, 100.0, 0.0), &line(0.0, 0.0, 0.0, 100.0), 0.05);
        assert!(hits.is_empty(), "{hits:?}");
    }

    #[test]
    fn collinear_connectors_do_not_cross() {
        let hits = crossings(&line(0.0, 0.0, 100.0, 0.0), &line(20.0, 0.0, 80.0, 0.0), 0.05);
        assert!(hits.is_empty(), "{hits:?}");
    }

    #[test]
    fn a_crossing_becomes_a_semicircular_hop() {
        let horizontal = line(-50.0, 0.0, 50.0, 0.0);
        let vertical = line(0.0, -50.0, 0.0, 50.0);
        let options = JumpOptions { radius: 6.0, ..JumpOptions::default() };
        let hopped = insert_jumps(&horizontal, std::slice::from_ref(&vertical), &options);

        assert_eq!(cubic_count(&hopped), 2, "{hopped:?}");
        assert_eq!(hopped.start, Point::new(-50.0, 0.0));
        assert_eq!(hopped.end(), Point::new(50.0, 0.0));

        // The arc rises exactly one radius above the line, and nothing dips below.
        let bounds = hopped.bounds();
        assert!((bounds.min.y - -6.0).abs() < 1e-3, "{bounds:?}");
        assert!(bounds.max.y.abs() < 1e-9);

        // A semicircle replaces a 2r straight run: πr − 2r longer. Measured against a
        // fine flattening, since the default tolerance under-measures a 6px arc. The
        // residual 0.003px is the cubic pair's ~0.03% approximation of a true circle.
        let expected = 100.0 + (std::f64::consts::PI - 2.0) * 6.0;
        let measured = hopped.flatten(1e-5).length();
        assert!((measured - expected).abs() < 0.01, "{measured} vs {expected}");
    }

    #[test]
    fn the_hop_stays_within_a_radius_of_the_crossing() {
        let hopped = insert_jumps(
            &line(-50.0, 0.0, 50.0, 0.0),
            &[line(0.0, -50.0, 0.0, 50.0)],
            &JumpOptions { radius: 6.0, ..JumpOptions::default() },
        );
        let bounds = hopped.bounds();
        assert!((bounds.min.x - -50.0).abs() < 1e-9 && (bounds.max.x - 50.0).abs() < 1e-9);
        // Everything more than a radius from the crossing is still on the line.
        assert!(hopped.hit_test(Point::new(-20.0, 0.0), 1e-6));
        assert!(hopped.hit_test(Point::new(20.0, 0.0), 1e-6));
    }

    #[test]
    fn several_crossings_each_get_their_own_hop() {
        let hopped = insert_jumps(
            &line(-50.0, 0.0, 50.0, 0.0),
            &[line(-20.0, -10.0, -20.0, 10.0), line(20.0, -10.0, 20.0, 10.0)],
            &JumpOptions::default(),
        );
        assert_eq!(cubic_count(&hopped), 4, "{hopped:?}");
    }

    #[test]
    fn crossings_closer_together_than_a_hop_are_merged_into_one() {
        let hopped = insert_jumps(
            &line(-50.0, 0.0, 50.0, 0.0),
            &[line(0.0, -10.0, 0.0, 10.0), line(3.0, -10.0, 3.0, 10.0)],
            &JumpOptions { radius: 6.0, ..JumpOptions::default() },
        );
        assert_eq!(cubic_count(&hopped), 2, "{hopped:?}");
    }

    #[test]
    fn a_crossing_with_no_room_for_an_arc_is_left_alone() {
        // The crossing is 2px from the end; a 6px hop would overrun it.
        let hopped = insert_jumps(
            &line(0.0, 0.0, 100.0, 0.0),
            &[line(2.0, -10.0, 2.0, 10.0)],
            &JumpOptions { radius: 6.0, ..JumpOptions::default() },
        );
        assert_eq!(cubic_count(&hopped), 0);
        assert_eq!(hopped.segments.len(), 1);
    }

    #[test]
    fn a_segment_shorter_than_a_hop_is_left_alone() {
        let hopped = insert_jumps(
            &line(0.0, 0.0, 8.0, 0.0),
            &[line(4.0, -10.0, 4.0, 10.0)],
            &JumpOptions { radius: 6.0, ..JumpOptions::default() },
        );
        assert_eq!(cubic_count(&hopped), 0);
    }

    #[test]
    fn hops_survive_a_corner_in_the_hopping_path() {
        let elbow = RoutedPath::from_points(
            [Point::new(0.0, 0.0), Point::new(100.0, 0.0), Point::new(100.0, 100.0)],
            ObstacleAvoidance::Cleared,
        );
        let hopped = insert_jumps(
            &elbow,
            &[line(50.0, -10.0, 50.0, 10.0), line(90.0, 50.0, 110.0, 50.0)],
            &JumpOptions::default(),
        );
        // One hop on each leg, and the corner itself is still there.
        assert_eq!(cubic_count(&hopped), 4, "{hopped:?}");
        assert_eq!(hopped.end(), Point::new(100.0, 100.0));
    }

    #[test]
    fn a_crossing_on_a_curved_segment_is_left_as_a_plain_crossing() {
        let curve = RoutedPath {
            start: Point::new(-50.0, 0.0),
            segments: vec![PathSegment::Cubic {
                control1: Point::new(-20.0, 40.0),
                control2: Point::new(20.0, 40.0),
                to: Point::new(50.0, 0.0),
            }],
            avoidance: ObstacleAvoidance::NotAttempted,
        };
        let hopped =
            insert_jumps(&curve, &[line(0.0, -50.0, 0.0, 50.0)], &JumpOptions::default());
        assert_eq!(hopped, curve, "curved segments pass through untouched");
    }

    #[test]
    fn a_straight_connector_hops_over_a_curved_one() {
        let curve = RoutedPath {
            start: Point::new(0.0, -50.0),
            segments: vec![PathSegment::Cubic {
                control1: Point::new(20.0, -20.0),
                control2: Point::new(20.0, 20.0),
                to: Point::new(0.0, 50.0),
            }],
            avoidance: ObstacleAvoidance::NotAttempted,
        };
        let hopped =
            insert_jumps(&line(-50.0, 0.0, 50.0, 0.0), &[curve], &JumpOptions::default());
        assert_eq!(cubic_count(&hopped), 2, "{hopped:?}");
    }

    #[test]
    fn a_zero_radius_leaves_every_path_untouched() {
        let straight = line(-50.0, 0.0, 50.0, 0.0);
        let hopped = insert_jumps(
            &straight,
            &[line(0.0, -50.0, 0.0, 50.0)],
            &JumpOptions { radius: 0.0, ..JumpOptions::default() },
        );
        assert_eq!(hopped, straight);
    }

    #[test]
    fn the_convention_makes_later_connectors_hop_over_earlier_ones() {
        let mut paths = vec![
            line(0.0, -50.0, 0.0, 50.0),
            line(-50.0, 0.0, 50.0, 0.0),
            line(-50.0, 20.0, 50.0, 20.0),
        ];
        apply_jump_overs(&mut paths, &JumpOptions::default());

        assert_eq!(cubic_count(&paths[0]), 0, "the first connector never hops");
        assert_eq!(cubic_count(&paths[1]), 2);
        assert_eq!(cubic_count(&paths[2]), 2);
    }

    #[test]
    fn a_connector_never_hops_over_an_arc_that_only_exists_because_of_it() {
        let mut paths = vec![line(0.0, -50.0, 0.0, 50.0), line(-50.0, 0.0, 50.0, 0.0)];
        apply_jump_overs(&mut paths, &JumpOptions::default());
        // Exactly one hop between them, not one plus a second over its own arc.
        assert_eq!(cubic_count(&paths[0]) + cubic_count(&paths[1]), 2);
    }
}
