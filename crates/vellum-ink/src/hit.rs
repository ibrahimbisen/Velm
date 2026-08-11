//! Where a stroke is, what it covers, and what the cursor is touching.
//!
//! All four operations here — bounds, distance, selection, erasing — reduce to one
//! question: how far is a point from the inked region? The inked region is not the
//! centreline. It is the centreline swept by a disc whose radius follows the stroke's
//! width, which is exactly what round caps and round joins mean geometrically. That
//! equivalence is worth stating once because it makes two otherwise fiddly problems
//! exact rather than approximate:
//!
//! - **Bounds.** The bounding box of a Minkowski sum with a disc is the bounding box
//!   of the centreline grown by the radius. With a varying radius it is the union
//!   over points of each point grown by *its own* radius, because the extreme of a
//!   convex hull in any direction is the extreme of one of the shapes it was built
//!   from. So the box below is tight, not conservative — no padding, no guessing.
//! - **Hit-testing.** The distance to one segment's swept region has a closed form
//!   (the "round cone" distance), so hit-testing is an exact signed distance rather
//!   than a sampled approximation. Sampling would make a hit test that misses near
//!   the middle of a long segment, which reads to the user as a stroke that cannot
//!   be clicked.
//!
//! Butt or square caps would break the first of those, which is why cap style is not
//! a tessellation option: the bounds and the mesh have to agree about what shape the
//! stroke is.

use crate::stroke::{Stroke, StrokePoint};
use crate::vec2::{Vec2, v};

/// An axis-aligned box in the stroke's own coordinate space, covering the *inked*
/// region rather than just the centreline.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bounds {
    pub min_x: f64,
    pub min_y: f64,
    pub max_x: f64,
    pub max_y: f64,
}

impl Bounds {
    pub fn width(&self) -> f64 {
        self.max_x - self.min_x
    }

    pub fn height(&self) -> f64 {
        self.max_y - self.min_y
    }

    pub fn center(&self) -> (f64, f64) {
        ((self.min_x + self.max_x) / 2.0, (self.min_y + self.max_y) / 2.0)
    }

    /// Edge-inclusive, matching `vellum_scene::WorldRect` so that a bound computed
    /// here and one stored in the spatial index cannot disagree about a click on the
    /// exact border.
    pub fn contains(&self, (x, y): (f64, f64)) -> bool {
        x >= self.min_x && x <= self.max_x && y >= self.min_y && y <= self.max_y
    }

    pub fn union(&self, other: &Self) -> Self {
        Self {
            min_x: self.min_x.min(other.min_x),
            min_y: self.min_y.min(other.min_y),
            max_x: self.max_x.max(other.max_x),
            max_y: self.max_y.max(other.max_y),
        }
    }
}

/// Where on a stroke's centreline a query point landed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Hit {
    /// Index of the segment, i.e. of its first point.
    pub segment: usize,
    /// Position along that segment, 0..=1.
    pub t: f64,
    /// The centreline point itself, with pressure interpolated — ready to become the
    /// join point of a split.
    pub position: StrokePoint,
    /// Distance from the query point to the **centreline**, ignoring width. Distinct
    /// from [`Stroke::signed_distance`], which measures to the stroke's edge; both
    /// are needed and confusing them puts the eraser half a stroke width out.
    pub centreline_distance: f64,
}

/// Exact bounds of the inked region, or `None` for a stroke with no points.
///
/// `None` rather than a zero-sized box at the origin: an empty stroke inserted into
/// the spatial index as a point at (0, 0) is a phantom item that swallows clicks in
/// the top-left corner of the board, and the cause of that is very hard to see.
pub fn bounds(stroke: &Stroke) -> Option<Bounds> {
    let mut points = stroke.points().iter();
    let first = points.next()?;
    let half = |p: &StrokePoint| stroke.width() * p.pressure / 2.0;

    let r = half(first);
    let mut b = Bounds {
        min_x: first.x - r,
        min_y: first.y - r,
        max_x: first.x + r,
        max_y: first.y + r,
    };
    for p in points {
        let r = half(p);
        b.min_x = b.min_x.min(p.x - r);
        b.min_y = b.min_y.min(p.y - r);
        b.max_x = b.max_x.max(p.x + r);
        b.max_y = b.max_y.max(p.y + r);
    }
    Some(b)
}

/// Distance from a point to a polyline, with the segment and parameter that achieved
/// it. Returns an infinite distance for fewer than two points *unless* there is
/// exactly one, in which case the single point is the answer.
pub(crate) fn distance_to_polyline(p: (f64, f64), points: &[StrokePoint]) -> (f64, usize, f64) {
    let q = v(p.0, p.1);
    match points {
        [] => (f64::INFINITY, 0, 0.0),
        [only] => ((q - Vec2::from_point(*only)).length(), 0, 0.0),
        _ => {
            let mut best = (f64::INFINITY, 0usize, 0.0);
            for (i, w) in points.windows(2).enumerate() {
                let (a, b) = (Vec2::from_point(w[0]), Vec2::from_point(w[1]));
                let ab = b - a;
                let length_squared = ab.length_squared();
                let t = if length_squared > 0.0 {
                    ((q - a).dot(ab) / length_squared).clamp(0.0, 1.0)
                } else {
                    0.0
                };
                let d = (q - (a + ab * t)).length();
                if d < best.0 {
                    best = (d, i, t);
                }
            }
            best
        }
    }
}

/// Signed distance from `p` to the region swept by a disc moving from `a` to `b`
/// while its radius goes from `ra` to `rb`. Negative inside.
///
/// This is Inigo Quilez's round-cone distance, in 2D and in f64. The three branches
/// are the two spherical caps and the tangent flank between them; which one applies
/// is decided by comparing against the tangent line's own slope, so the result is
/// exact everywhere rather than only near the middle. When `ra == rb` it degenerates
/// to the familiar capsule distance, which is what the unit tests pin it against.
fn round_cone_distance(p: Vec2, a: Vec2, b: Vec2, ra: f64, rb: f64) -> f64 {
    let ba = b - a;
    let l2 = ba.length_squared();
    if l2 <= 0.0 {
        return (p - a).length() - ra.max(rb);
    }

    let rr = ra - rb;
    let a2 = l2 - rr * rr;
    if a2 <= 0.0 {
        // The radii differ by more than the centres do, so one disc contains the
        // other and the hull is just the larger disc. The tangent branch below has
        // no real solution here and would take a square root of a negative number.
        return ((p - a).length() - ra).min((p - b).length() - rb);
    }

    let il2 = 1.0 / l2;
    let pa = p - a;
    let y = pa.dot(ba);
    let z = y - l2;
    let x2 = (pa * l2 - ba * y).length_squared();
    let y2 = y * y * l2;
    let z2 = z * z * l2;

    let k = sign(rr) * rr * rr * x2;
    if sign(z) * a2 * z2 > k {
        return (x2 + z2).sqrt() * il2 - rb;
    }
    if sign(y) * a2 * y2 < k {
        return (x2 + y2).sqrt() * il2 - ra;
    }
    ((x2 * a2 * il2).sqrt() + y * rr) * il2 - ra
}

/// Zero maps to zero, unlike [`f64::signum`], which returns ±1 for ±0.0. The
/// round-cone branch selection relies on the three-way version.
fn sign(x: f64) -> f64 {
    if x > 0.0 {
        1.0
    } else if x < 0.0 {
        -1.0
    } else {
        0.0
    }
}

impl Stroke {
    /// Exact bounds of the inked region. See [`bounds`].
    pub fn bounds(&self) -> Option<Bounds> {
        bounds(self)
    }

    /// Signed distance from `point` to the stroke's edge: negative inside the ink,
    /// zero on its outline, positive outside. `None` for an empty stroke.
    ///
    /// Taking the minimum over segments gives the exact distance everywhere outside
    /// the stroke, which is all a hit test reads. Inside a self-overlap it
    /// under-estimates the depth — the true distance to the union's boundary can be
    /// larger than the distance to the nearest single segment's boundary — but the
    /// sign is still right, so nothing that depends on inside-ness is affected.
    pub fn signed_distance(&self, point: (f64, f64)) -> Option<f64> {
        let points = self.points();
        let p = v(point.0, point.1);
        let radius = |q: &StrokePoint| self.width() * q.pressure / 2.0;

        match points {
            [] => None,
            [only] => Some((p - Vec2::from_point(*only)).length() - radius(only)),
            _ => Some(
                points
                    .windows(2)
                    .map(|w| {
                        round_cone_distance(
                            p,
                            Vec2::from_point(w[0]),
                            Vec2::from_point(w[1]),
                            radius(&w[0]),
                            radius(&w[1]),
                        )
                    })
                    .fold(f64::INFINITY, f64::min),
            ),
        }
    }

    /// Whether `point` selects this stroke, allowing `tolerance` world px of slack
    /// beyond the ink itself.
    ///
    /// Slack is needed because a hairline stroke is otherwise almost impossible to
    /// click: at a 1px width the target is half a pixel wide. Callers should scale
    /// the tolerance by the inverse zoom so the grab area stays constant on screen.
    pub fn hit_test(&self, point: (f64, f64), tolerance: f64) -> bool {
        self.signed_distance(point).is_some_and(|d| d <= tolerance.max(0.0))
    }

    /// The closest point on the centreline, or `None` for an empty stroke.
    pub fn closest_point(&self, point: (f64, f64)) -> Option<Hit> {
        let points = self.points();
        if points.is_empty() {
            return None;
        }
        let (centreline_distance, segment, t) = distance_to_polyline(point, points);
        let position = if points.len() == 1 {
            points[0]
        } else {
            points[segment].lerp(points[segment + 1], t)
        };
        Some(Hit { segment, t, position, centreline_distance })
    }

    /// Cuts the stroke in two at the point nearest `at`, for a partial erase.
    ///
    /// Returns `None` when `at` does not touch the stroke, when the stroke is a dot
    /// (there is nothing to cut), or when the cut lands so close to an end that one
    /// side would collapse to a single point. That last case falls out of
    /// [`Stroke::new`]'s coincident-point merging rather than needing a threshold of
    /// its own, which keeps the two rules from drifting apart.
    ///
    /// Both halves keep the original width and the interpolated pressure at the cut,
    /// so a split is invisible until the pieces are moved.
    pub fn split_at(&self, at: (f64, f64), tolerance: f64) -> Option<(Stroke, Stroke)> {
        if self.len() < 2 || !self.hit_test(at, tolerance) {
            return None;
        }
        let hit = self.closest_point(at)?;
        let points = self.points();

        let mut head = points[..=hit.segment].to_vec();
        head.push(hit.position);
        let mut tail = vec![hit.position];
        tail.extend_from_slice(&points[hit.segment + 1..]);

        let (head, tail) = (self.with_points(head), self.with_points(tail));
        if head.len() < 2 || tail.len() < 2 {
            return None;
        }
        Some((head, tail))
    }

    /// Removes everything within `radius` of `center` and returns what is left, in
    /// order along the original stroke.
    ///
    /// Zero, one or two pieces normally; more if the stroke crosses the eraser
    /// several times, which handwriting does constantly. Pieces that would collapse
    /// to a single point are dropped rather than left as specks.
    ///
    /// The test is against the **centreline**: the eraser has to touch the line
    /// itself, not merely overlap its edge. That is what makes erasing a thick
    /// stroke feel controllable — with surface semantics a 20px stroke would start
    /// vanishing 10px before the cursor reached it. A caller that wants surface
    /// semantics can pass `radius + stroke.max_half_width()`.
    pub fn erase(&self, center: (f64, f64), radius: f64) -> Vec<Stroke> {
        let points = self.points();
        let c = v(center.0, center.1);
        let radius = radius.max(0.0);
        let covers = |p: Vec2| (p - c).length_squared() <= radius * radius;

        match points {
            [] => return Vec::new(),
            [only] => {
                return if covers(Vec2::from_point(*only)) {
                    Vec::new()
                } else {
                    vec![self.clone()]
                };
            }
            _ => {}
        }

        let mut pieces: Vec<Stroke> = Vec::new();
        let mut current: Vec<StrokePoint> = Vec::new();

        for w in points.windows(2) {
            let (p, q) = (w[0], w[1]);
            // Split the segment at its circle crossings, then classify each stretch
            // by its midpoint. Classifying stretches rather than tracking an
            // inside/outside flag across crossings means a vertex that lands exactly
            // on the eraser's rim cannot flip the state machine out of step.
            let mut cuts = vec![0.0];
            cuts.extend(circle_crossings(p, q, c, radius));
            cuts.push(1.0);

            for pair in cuts.windows(2) {
                let (t0, t1) = (pair[0], pair[1]);
                if t1 <= t0 {
                    continue;
                }
                let start = p.lerp(q, t0);
                if covers(Vec2::from_point(p.lerp(q, (t0 + t1) / 2.0))) {
                    flush(&mut current, self, &mut pieces);
                } else {
                    if current.is_empty() {
                        current.push(start);
                    }
                    current.push(p.lerp(q, t1));
                }
            }
        }
        flush(&mut current, self, &mut pieces);
        pieces
    }
}

fn flush(current: &mut Vec<StrokePoint>, source: &Stroke, pieces: &mut Vec<Stroke>) {
    if current.len() >= 2 {
        let piece = source.with_points(current.iter().copied());
        if piece.len() >= 2 {
            pieces.push(piece);
        }
    }
    current.clear();
}

/// Parameters in `(0, 1)` where the segment `p→q` crosses the circle, ascending.
fn circle_crossings(p: StrokePoint, q: StrokePoint, c: Vec2, radius: f64) -> Vec<f64> {
    let a = Vec2::from_point(p);
    let d = Vec2::from_point(q) - a;
    let f = d.length_squared();
    if f <= 0.0 {
        return Vec::new();
    }
    let ac = a - c;
    let g = d.dot(ac);
    let h = ac.length_squared() - radius * radius;
    let discriminant = g * g - f * h;
    if discriminant <= 0.0 {
        return Vec::new();
    }
    let root = discriminant.sqrt();
    [(-g - root) / f, (-g + root) / f]
        .into_iter()
        .filter(|t| *t > 0.0 && *t < 1.0)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::simplify::point_to_segment_distance;

    fn xy(points: &[(f64, f64)]) -> Stroke {
        Stroke::from_miro(points, Some(10.0))
    }

    /// Independent capsule distance, used to check the round-cone formula in the
    /// constant-radius case it has to reduce to.
    fn capsule_distance(p: Vec2, a: Vec2, b: Vec2, r: f64) -> f64 {
        point_to_segment_distance(p, a, b) - r
    }

    #[test]
    fn an_empty_stroke_has_no_bounds_and_cannot_be_hit() {
        let s = xy(&[]);
        assert_eq!(s.bounds(), None);
        assert_eq!(s.signed_distance((0.0, 0.0)), None);
        assert!(!s.hit_test((0.0, 0.0), 100.0));
        assert_eq!(s.closest_point((0.0, 0.0)), None);
    }

    #[test]
    fn a_dot_is_a_disc_of_the_stroke_width() {
        let s = xy(&[(100.0, 50.0)]);
        let b = s.bounds().unwrap();
        assert_eq!((b.min_x, b.min_y, b.max_x, b.max_y), (95.0, 45.0, 105.0, 55.0));
        assert_eq!(b.width(), 10.0);
        assert_eq!(b.center(), (100.0, 50.0));

        assert_eq!(s.signed_distance((100.0, 50.0)), Some(-5.0));
        assert_eq!(s.signed_distance((110.0, 50.0)), Some(5.0));
        assert!(s.hit_test((104.9, 50.0), 0.0));
        assert!(!s.hit_test((106.0, 50.0), 0.0));
        assert!(s.hit_test((106.0, 50.0), 2.0));
    }

    /// Round caps make the box exact: it is the centreline's box grown by the
    /// radius, with nothing left over at the ends.
    #[test]
    fn bounds_grow_by_exactly_the_half_width() {
        let s = xy(&[(0.0, 0.0), (100.0, 40.0)]);
        let b = s.bounds().unwrap();
        assert_eq!((b.min_x, b.min_y, b.max_x, b.max_y), (-5.0, -5.0, 105.0, 45.0));
    }

    #[test]
    fn variable_width_bounds_use_each_points_own_radius() {
        let s = Stroke::new(
            [
                StrokePoint::with_pressure(0.0, 0.0, 0.2),
                StrokePoint::with_pressure(100.0, 0.0, 2.0),
            ],
            10.0,
        );
        let b = s.bounds().unwrap();
        assert_eq!((b.min_x, b.max_x), (-1.0, 110.0));
        assert_eq!((b.min_y, b.max_y), (-10.0, 10.0));
    }

    #[test]
    fn bounds_union_covers_both() {
        let a = xy(&[(0.0, 0.0)]).bounds().unwrap();
        let b = xy(&[(100.0, 100.0)]).bounds().unwrap();
        let u = a.union(&b);
        assert!(u.contains((-5.0, -5.0)) && u.contains((105.0, 105.0)));
    }

    /// The round-cone distance must agree with an independently written capsule
    /// distance wherever the radius is constant — inside, outside, past both ends
    /// and level with the flank.
    #[test]
    fn constant_radius_matches_a_capsule() {
        let (a, b, r) = (v(10.0, 10.0), v(90.0, 40.0), 7.0);
        for x in (-40..140).step_by(3) {
            for y in (-40..100).step_by(3) {
                let p = v(x as f64, y as f64);
                let expected = capsule_distance(p, a, b, r);
                let got = round_cone_distance(p, a, b, r, r);
                assert!((got - expected).abs() < 1e-9, "at {p:?}: {got} vs {expected}");
            }
        }
    }

    /// With a varying radius there is no simpler closed form to check against, so
    /// the invariant is checked instead: a point exactly `d` outside the surface
    /// must report `d`, which is verified by walking outwards from the surface along
    /// the gradient of the distance field.
    #[test]
    fn varying_radius_distance_is_consistent_with_itself() {
        let (a, b, ra, rb) = (v(0.0, 0.0), v(120.0, 0.0), 4.0, 20.0);
        for x in (-40..160).step_by(7) {
            for y in (-60..60).step_by(7) {
                let p = v(x as f64, y as f64);
                let d = round_cone_distance(p, a, b, ra, rb);
                if d < 1.0 {
                    continue;
                }
                // Numerical gradient; a true distance field has |∇d| == 1 outside.
                let e = 1e-4;
                let gx = (round_cone_distance(v(p.x + e, p.y), a, b, ra, rb)
                    - round_cone_distance(v(p.x - e, p.y), a, b, ra, rb))
                    / (2.0 * e);
                let gy = (round_cone_distance(v(p.x, p.y + e), a, b, ra, rb)
                    - round_cone_distance(v(p.x, p.y - e), a, b, ra, rb))
                    / (2.0 * e);
                assert!(
                    (gx.hypot(gy) - 1.0).abs() < 1e-3,
                    "gradient at {p:?} is {}",
                    gx.hypot(gy)
                );
            }
        }
    }

    #[test]
    fn hit_testing_respects_the_stroke_width_not_just_the_line() {
        let s = xy(&[(0.0, 0.0), (100.0, 0.0)]);
        assert!(s.hit_test((50.0, 4.9), 0.0));
        assert!(!s.hit_test((50.0, 5.1), 0.0));
        // Round caps: reachable beyond the last point, but only by the radius.
        assert!(s.hit_test((104.9, 0.0), 0.0));
        assert!(!s.hit_test((105.1, 0.0), 0.0));
        // A square cap would claim this corner; a round one does not.
        assert!(!s.hit_test((104.0, 4.0), 0.0));
    }

    #[test]
    fn hit_testing_a_self_intersecting_stroke_works_on_both_branches() {
        let s = xy(&[(0.0, 0.0), (100.0, 100.0), (100.0, 0.0), (0.0, 100.0)]);
        assert!(s.hit_test((50.0, 50.0), 0.0));
        assert!(s.hit_test((25.0, 25.0), 0.0));
        assert!(s.hit_test((25.0, 75.0), 0.0));
        assert!(!s.hit_test((10.0, 50.0), 0.0));
    }

    #[test]
    fn closest_point_finds_the_segment_and_parameter() {
        let s = xy(&[(0.0, 0.0), (100.0, 0.0), (100.0, 100.0)]);
        let hit = s.closest_point((100.0, 25.0)).unwrap();
        assert_eq!(hit.segment, 1);
        assert!((hit.t - 0.25).abs() < 1e-12);
        assert_eq!((hit.position.x, hit.position.y), (100.0, 25.0));
        assert_eq!(hit.centreline_distance, 0.0);
    }

    #[test]
    fn splitting_produces_two_strokes_that_meet_at_the_cut() {
        let s = xy(&[(0.0, 0.0), (100.0, 0.0), (100.0, 100.0)]);
        let (head, tail) = s.split_at((50.0, 2.0), 0.0).unwrap();
        assert_eq!(head.points().last().unwrap(), tail.points().first().unwrap());
        assert_eq!((head.points().last().unwrap().x, head.points().last().unwrap().y), (50.0, 0.0));
        assert_eq!(head.len(), 2);
        assert_eq!(tail.len(), 3);
        assert_eq!(head.width(), s.width());
        // The pieces together still cover the original centreline length.
        assert!((head.length() + tail.length() - s.length()).abs() < 1e-9);
    }

    #[test]
    fn splitting_misses_and_degenerate_cases_return_none() {
        let s = xy(&[(0.0, 0.0), (100.0, 0.0)]);
        assert_eq!(s.split_at((50.0, 500.0), 0.0), None, "a miss is not a split");
        assert_eq!(s.split_at((0.0, 0.0), 0.0), None, "cutting at the very start");
        assert_eq!(s.split_at((100.0, 0.0), 0.0), None, "cutting at the very end");
        assert_eq!(xy(&[(0.0, 0.0)]).split_at((0.0, 0.0), 0.0), None, "a dot cannot split");
        assert_eq!(xy(&[]).split_at((0.0, 0.0), 0.0), None);
    }

    #[test]
    fn erasing_the_middle_leaves_two_pieces() {
        let s = xy(&[(0.0, 0.0), (100.0, 0.0)]);
        let pieces = s.erase((50.0, 0.0), 10.0);
        assert_eq!(pieces.len(), 2);
        assert_eq!(pieces[0].points()[1].x, 40.0);
        assert_eq!(pieces[1].points()[0].x, 60.0);
    }

    #[test]
    fn erasing_an_end_leaves_one_shorter_piece() {
        let s = xy(&[(0.0, 0.0), (100.0, 0.0)]);
        let pieces = s.erase((0.0, 0.0), 25.0);
        assert_eq!(pieces.len(), 1);
        assert_eq!(pieces[0].points()[0].x, 25.0);
        assert_eq!(pieces[0].points()[1].x, 100.0);
    }

    #[test]
    fn an_eraser_that_covers_everything_leaves_nothing() {
        let s = xy(&[(0.0, 0.0), (50.0, 50.0), (100.0, 0.0)]);
        assert!(s.erase((50.0, 25.0), 1000.0).is_empty());
        assert!(xy(&[(0.0, 0.0)]).erase((0.0, 0.0), 1.0).is_empty());
    }

    #[test]
    fn an_eraser_that_misses_changes_nothing() {
        let s = xy(&[(0.0, 0.0), (50.0, 50.0), (100.0, 0.0)]);
        let pieces = s.erase((500.0, 500.0), 10.0);
        assert_eq!(pieces.len(), 1);
        assert_eq!(pieces[0], s);
    }

    /// A stroke can pass through the eraser more than once — every `e`, `l` and `t`
    /// in handwriting does. Each crossing has to cut.
    #[test]
    fn a_stroke_crossing_the_eraser_twice_yields_three_pieces() {
        let s = xy(&[(0.0, 0.0), (0.0, 100.0), (60.0, 100.0), (60.0, 0.0), (120.0, 0.0)]);
        let pieces = s.erase((30.0, 50.0), 40.0);
        assert_eq!(pieces.len(), 3, "{pieces:#?}");
        assert!(pieces.iter().all(|p| p.len() >= 2));
    }

    /// Erasing exactly across a vertex must not leave a stray fragment behind: the
    /// vertex is inside the disc and has to go with it.
    #[test]
    fn erasing_over_a_vertex_removes_it() {
        let s = xy(&[(0.0, 0.0), (50.0, 0.0), (100.0, 0.0)]);
        let pieces = s.erase((50.0, 0.0), 20.0);
        assert_eq!(pieces.len(), 2);
        let surviving = pieces.iter().flat_map(|p| p.points());
        assert!(surviving.map(|q| (q.x - 50.0).abs()).all(|d| d >= 20.0 - 1e-9));
    }

    #[test]
    fn erasing_within_a_single_segment_splits_that_segment() {
        let s = xy(&[(0.0, 0.0), (200.0, 0.0)]);
        let pieces = s.erase((100.0, 0.0), 5.0);
        assert_eq!(pieces.len(), 2);
        assert_eq!(pieces[0].length(), 95.0);
        assert_eq!(pieces[1].length(), 95.0);
    }

    #[test]
    fn a_very_large_stroke_bounds_and_hit_tests_without_trouble() {
        let points: Vec<_> = (0..20_000)
            .map(|i| {
                let t = i as f64 * 0.01;
                (t.cos() * 5000.0 + t, t.sin() * 5000.0)
            })
            .collect();
        let s = xy(&points);
        let b = s.bounds().unwrap();
        assert!(b.width() > 9000.0 && b.height() > 9000.0);
        assert!(s.hit_test((s.points()[9999].x, s.points()[9999].y), 0.0));
        assert!(!s.hit_test((0.0, 0.0), 0.0));
    }
}
