//! The one geometric representation every shape reduces to.
//!
//! [`Shape::outline`](crate::Shape::outline) is the only per-shape geometry code in
//! the crate. Bounds, hit-testing, connector anchors, the `lyon` path and both
//! meshes are all derived from the [`Outline`] it returns, so adding a shape means
//! describing its contours once rather than teaching six subsystems about it.
//!
//! An outline separates two kinds of contour, because flowchart symbols need both:
//!
//! - [`Outline::contours`] are the silhouette — what gets filled, hit-tested and
//!   anchored against.
//! - [`Outline::details`] are interior decoration: the two side bars of a
//!   predefined process, the front rim of a cylinder, the cross inside a summing
//!   junction. They are stroked but never filled, and never affect `contains`,
//!   because a click in the middle of a predefined process is a hit on the process,
//!   not a miss between two lines.

use crate::unit::{Bounds, Point};
use lyon::geom::{CubicBezierSegment, QuadraticBezierSegment};
use lyon::path::Path;

/// How accurately curves are flattened for hit-testing and bounds.
///
/// `5e-4` of the box: half a pixel on a 1000px shape, which is finer than any
/// click a user can place. Tessellation takes its tolerance from the caller
/// instead, since that one trades vertex count against on-screen size.
pub const HIT_TEST_TOLERANCE: f32 = 5e-4;

/// One segment of a contour, always continuing from the previous endpoint.
///
/// Arcs are stored as cubics rather than as an arc primitive: `lyon` flattens,
/// bounds and tessellates cubics natively, and a circular arc is reproduced by a
/// cubic to within 2e-4 of its radius per 90° quadrant — an order of magnitude
/// below the hit-test tolerance, and invisible at any zoom.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Segment {
    Line { to: Point },
    Quadratic { ctrl: Point, to: Point },
    Cubic { ctrl1: Point, ctrl2: Point, to: Point },
}

impl Segment {
    pub fn end(&self) -> Point {
        match *self {
            Self::Line { to } | Self::Quadratic { to, .. } | Self::Cubic { to, .. } => to,
        }
    }

    /// The tight bounding box of the segment, including curve extrema rather than
    /// just control points. Shapes are normalised against these bounds, so a loose
    /// box would leave every curved shape slightly short of its item rect.
    fn bounds(&self, from: Point) -> Bounds {
        match *self {
            Self::Line { to } => Bounds::new(from, from).expanded_to(to),
            Self::Quadratic { ctrl, to } => {
                let b = QuadraticBezierSegment {
                    from: from.to_lyon(),
                    ctrl: ctrl.to_lyon(),
                    to: to.to_lyon(),
                }
                .bounding_box();
                Bounds::new(Point::from_lyon(b.min), Point::from_lyon(b.max))
            }
            Self::Cubic { ctrl1, ctrl2, to } => {
                let b = CubicBezierSegment {
                    from: from.to_lyon(),
                    ctrl1: ctrl1.to_lyon(),
                    ctrl2: ctrl2.to_lyon(),
                    to: to.to_lyon(),
                }
                .bounding_box();
                Bounds::new(Point::from_lyon(b.min), Point::from_lyon(b.max))
            }
        }
    }

    fn flatten_into(&self, from: Point, tolerance: f32, out: &mut Vec<Point>) {
        match *self {
            Self::Line { to } => out.push(to),
            Self::Quadratic { ctrl, to } => QuadraticBezierSegment {
                from: from.to_lyon(),
                ctrl: ctrl.to_lyon(),
                to: to.to_lyon(),
            }
            .for_each_flattened(tolerance, &mut |line| out.push(Point::from_lyon(line.to))),
            Self::Cubic { ctrl1, ctrl2, to } => CubicBezierSegment {
                from: from.to_lyon(),
                ctrl1: ctrl1.to_lyon(),
                ctrl2: ctrl2.to_lyon(),
                to: to.to_lyon(),
            }
            .for_each_flattened(tolerance, &mut |line| out.push(Point::from_lyon(line.to))),
        }
    }

    fn map(&self, f: impl Fn(Point) -> Point) -> Self {
        match *self {
            Self::Line { to } => Self::Line { to: f(to) },
            Self::Quadratic { ctrl, to } => Self::Quadratic { ctrl: f(ctrl), to: f(to) },
            Self::Cubic { ctrl1, ctrl2, to } => {
                Self::Cubic { ctrl1: f(ctrl1), ctrl2: f(ctrl2), to: f(to) }
            }
        }
    }
}

/// A single sub-path.
///
/// Silhouette contours are always closed and always wound **clockwise on screen**
/// (positive shoelace area with y pointing down). Consistent winding is what lets
/// multi-part shapes such as a multi-document stack fill as a union under the
/// non-zero rule instead of punching each other out; `orientation_is_clockwise` is
/// asserted for every shape in the test suite so a new shape cannot get it wrong
/// silently.
#[derive(Debug, Clone, PartialEq)]
pub struct Contour {
    pub start: Point,
    pub segments: Vec<Segment>,
    pub closed: bool,
}

impl Contour {
    /// A closed straight-edged contour through `points`.
    pub fn polygon(points: &[Point]) -> Self {
        let (start, rest) = points.split_first().expect("a polygon needs at least one point");
        Self {
            start: *start,
            segments: rest.iter().map(|&to| Segment::Line { to }).collect(),
            closed: true,
        }
    }

    /// An open two-point contour, used for interior detail lines.
    pub fn line(from: Point, to: Point) -> Self {
        Self { start: from, segments: vec![Segment::Line { to }], closed: false }
    }

    pub fn builder(start: Point) -> ContourBuilder {
        ContourBuilder { start, segments: Vec::new(), cursor: start }
    }

    /// Endpoints only, skipping control points.
    pub fn endpoints(&self) -> impl Iterator<Item = Point> + '_ {
        std::iter::once(self.start).chain(self.segments.iter().map(Segment::end))
    }

    /// The contour as a polyline. The closing edge back to `start` is implied, not
    /// repeated, which is what both the winding test and `lyon` expect.
    pub fn flatten(&self, tolerance: f32) -> Vec<Point> {
        let mut out = vec![self.start];
        let mut cursor = self.start;
        for segment in &self.segments {
            segment.flatten_into(cursor, tolerance, &mut out);
            cursor = segment.end();
        }
        out
    }

    pub fn bounds(&self) -> Bounds {
        let mut bounds = Bounds::new(self.start, self.start);
        let mut cursor = self.start;
        for segment in &self.segments {
            bounds = bounds.union(segment.bounds(cursor));
            cursor = segment.end();
        }
        bounds
    }

    /// Twice the signed area of the flattened contour. Positive is clockwise on
    /// screen, because y points down.
    pub fn signed_area(&self) -> f32 {
        let points = self.flatten(HIT_TEST_TOLERANCE);
        let mut area = 0.0;
        for i in 0..points.len() {
            let a = points[i];
            let b = points[(i + 1) % points.len()];
            area += a.cross(b);
        }
        area
    }

    pub fn map(&self, f: &impl Fn(Point) -> Point) -> Self {
        Self {
            start: f(self.start),
            segments: self.segments.iter().map(|s| s.map(f)).collect(),
            closed: self.closed,
        }
    }

    /// The contour's corners if it is made entirely of straight edges, else `None`.
    /// This is the test that decides whether a shape can use the polygon SDF.
    pub fn as_polygon(&self) -> Option<Vec<Point>> {
        self.segments
            .iter()
            .all(|s| matches!(s, Segment::Line { .. }))
            .then(|| self.endpoints().collect())
    }
}

/// Builds a contour segment by segment, tracking the current point so shape
/// definitions read as a drawing sequence.
pub struct ContourBuilder {
    start: Point,
    segments: Vec<Segment>,
    cursor: Point,
}

impl ContourBuilder {
    pub fn line_to(&mut self, to: Point) -> &mut Self {
        self.segments.push(Segment::Line { to });
        self.cursor = to;
        self
    }

    pub fn quad_to(&mut self, ctrl: Point, to: Point) -> &mut Self {
        self.segments.push(Segment::Quadratic { ctrl, to });
        self.cursor = to;
        self
    }

    pub fn cubic_to(&mut self, ctrl1: Point, ctrl2: Point, to: Point) -> &mut Self {
        self.segments.push(Segment::Cubic { ctrl1, ctrl2, to });
        self.cursor = to;
        self
    }

    /// An elliptical arc about `centre` with radii `(rx, ry)`, sweeping from
    /// `from_deg` to `to_deg`.
    ///
    /// **Angles are degrees clockwise from east**, which is what "clockwise" means
    /// on screen once y points down; `0°` is the right edge, `90°` the bottom.
    ///
    /// Each 90° of sweep becomes one cubic with control handles of length
    /// `k = 4/3 · tan(Δ/4)` along the tangent — the standard circular-arc
    /// approximation, exact at both endpoints and at the midpoint.
    pub fn arc(&mut self, centre: Point, rx: f32, ry: f32, from_deg: f32, to_deg: f32) -> &mut Self {
        let sweep = to_deg - from_deg;
        let steps = (sweep.abs() / 90.0).ceil().max(1.0) as usize;
        let step = sweep / steps as f32;
        let k = 4.0 / 3.0 * (step.to_radians() / 4.0).tan();
        for i in 0..steps {
            let a0 = (from_deg + step * i as f32).to_radians();
            let a1 = (from_deg + step * (i + 1) as f32).to_radians();
            let at = |a: f32| Point::new(centre.x + rx * a.cos(), centre.y + ry * a.sin());
            let tangent = |a: f32| Point::new(-rx * a.sin(), ry * a.cos());
            let p0 = at(a0);
            let p1 = at(a1);
            self.segments.push(Segment::Cubic {
                ctrl1: p0 + tangent(a0) * k,
                ctrl2: p1 - tangent(a1) * k,
                to: p1,
            });
            self.cursor = p1;
        }
        self
    }

    /// Closes the contour. The closing edge back to the start point is implicit.
    pub fn close(&mut self) -> Contour {
        Contour {
            start: self.start,
            segments: std::mem::take(&mut self.segments),
            closed: true,
        }
    }

    /// Ends the contour without closing it — for detail strokes.
    pub fn end_open(&mut self) -> Contour {
        Contour {
            start: self.start,
            segments: std::mem::take(&mut self.segments),
            closed: false,
        }
    }
}

/// A shape's complete geometry in the unit box.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Outline {
    /// Filled, hit-tested and anchored. Ordered back to front, so a renderer that
    /// fills and strokes each contour in turn gets the layered look of a
    /// multi-document stack; tessellating them together instead unions them.
    pub contours: Vec<Contour>,
    /// Stroked only. Never filled, never hit-tested.
    pub details: Vec<Contour>,
}

impl Outline {
    pub fn new(contours: Vec<Contour>) -> Self {
        Self { contours, details: Vec::new() }
    }

    pub fn from_contour(contour: Contour) -> Self {
        Self::new(vec![contour])
    }

    pub fn polygon(points: &[Point]) -> Self {
        Self::from_contour(Contour::polygon(points))
    }

    pub fn with_details(mut self, details: Vec<Contour>) -> Self {
        self.details = details;
        self
    }

    /// Exact bounds of the silhouette, curve extrema included.
    ///
    /// Detail contours are excluded by construction — they live inside the
    /// silhouette — and including them would let a stray decoration change how the
    /// shape maps onto its item rect.
    pub fn bounds(&self) -> Bounds {
        self.contours
            .iter()
            .map(Contour::bounds)
            .reduce(Bounds::union)
            .unwrap_or(Bounds::new(Point::default(), Point::default()))
    }

    pub fn map(&self, f: impl Fn(Point) -> Point) -> Self {
        Self {
            contours: self.contours.iter().map(|c| c.map(&f)).collect(),
            details: self.details.iter().map(|c| c.map(&f)).collect(),
        }
    }

    /// Rescales the outline so its silhouette exactly fills the unit box.
    ///
    /// Applied to every shape, for one reason: a caller that maps the unit box onto
    /// an item rect must get an item of exactly that size. Shapes whose natural
    /// construction is a circle sector or a bezier heart do not land on `0..1` by
    /// themselves, and a shape that filled 96% of its box would sit visibly loose
    /// inside its selection handles. For shapes already defined on `0..1` — most of
    /// them — this is the identity map.
    pub fn normalised(&self) -> Self {
        let bounds = self.bounds();
        let (w, h) = (bounds.width(), bounds.height());
        if w <= f32::EPSILON || h <= f32::EPSILON {
            return self.clone();
        }
        self.map(move |p| Point::new((p.x - bounds.min.x) / w, (p.y - bounds.min.y) / h))
    }

    /// The silhouette as polylines, at hit-test accuracy.
    pub fn flattened(&self) -> Vec<Vec<Point>> {
        self.contours.iter().map(|c| c.flatten(HIT_TEST_TOLERANCE)).collect()
    }

    /// Non-zero winding number of the silhouette around `p`.
    ///
    /// Non-zero rather than even-odd so that overlapping parts of a multi-contour
    /// shape read as one solid body, matching how the fill is tessellated.
    pub fn winding(&self, p: Point) -> i32 {
        let mut winding = 0;
        for contour in self.flattened() {
            for i in 0..contour.len() {
                let a = contour[i];
                let b = contour[(i + 1) % contour.len()];
                // Half-open edge test: an edge is counted at its lower endpoint and
                // not at its upper one, so a ray passing exactly through a vertex is
                // counted once rather than twice or zero times.
                if a.y <= p.y {
                    if b.y > p.y && (b - a).cross(p - a) > 0.0 {
                        winding += 1;
                    }
                } else if b.y <= p.y && (b - a).cross(p - a) < 0.0 {
                    winding -= 1;
                }
            }
        }
        winding
    }

    /// Is `p` inside the shape?
    ///
    /// This is the hit-test the canvas uses, and it is a real point-in-polygon test
    /// rather than a bounding-box check: roughly two thirds of a five-pointed star's
    /// box is *not* the star, and clicking there must select what is behind it.
    pub fn contains(&self, p: Point) -> bool {
        self.winding(p) != 0
    }

    /// The furthest point at which the ray `origin + t·direction`, `t ≥ 0`, leaves
    /// the silhouette. Used to place connector anchors on the perimeter.
    ///
    /// Furthest rather than nearest because a ray cast outwards from the centre of
    /// a concave shape can cross the boundary several times, and the anchor belongs
    /// on the outside of the shape, not on the near wall of an internal notch.
    pub fn ray_exit(&self, origin: Point, direction: Point) -> Option<Point> {
        let dir = direction.normalised();
        let mut furthest: Option<f32> = None;
        for contour in self.flattened() {
            for i in 0..contour.len() {
                let a = contour[i];
                let b = contour[(i + 1) % contour.len()];
                let edge = b - a;
                let denom = dir.cross(edge);
                if denom.abs() < 1e-9 {
                    continue;
                }
                let w = a - origin;
                let t = w.cross(edge) / denom;
                let u = w.cross(dir) / denom;
                if t >= 0.0 && (0.0..=1.0).contains(&u) && furthest.is_none_or(|best| t > best) {
                    furthest = Some(t);
                }
            }
        }
        furthest.map(|t| origin + dir * t)
    }

    /// The point on the silhouette closest to `p`.
    pub fn nearest_point(&self, p: Point) -> Point {
        let mut best = (f32::INFINITY, p);
        for contour in self.flattened() {
            for i in 0..contour.len() {
                let a = contour[i];
                let b = contour[(i + 1) % contour.len()];
                let edge = b - a;
                let len_sq = edge.dot(edge);
                let candidate = if len_sq <= f32::EPSILON {
                    a
                } else {
                    a + edge * ((p - a).dot(edge) / len_sq).clamp(0.0, 1.0)
                };
                let distance = candidate.distance(p);
                if distance < best.0 {
                    best = (distance, candidate);
                }
            }
        }
        best.1
    }

    /// The silhouette as a `lyon` path — what gets filled.
    pub fn fill_path(&self) -> Path {
        self.build_path(false)
    }

    /// Silhouette plus interior detail lines — what gets stroked.
    pub fn stroke_path(&self) -> Path {
        self.build_path(true)
    }

    fn build_path(&self, with_details: bool) -> Path {
        let mut builder = Path::builder();
        let details: &[Contour] = if with_details { &self.details } else { &[] };
        for contour in self.contours.iter().chain(details) {
            builder.begin(contour.start.to_lyon());
            for segment in &contour.segments {
                match *segment {
                    Segment::Line { to } => {
                        builder.line_to(to.to_lyon());
                    }
                    Segment::Quadratic { ctrl, to } => {
                        builder.quadratic_bezier_to(ctrl.to_lyon(), to.to_lyon());
                    }
                    Segment::Cubic { ctrl1, ctrl2, to } => {
                        builder.cubic_bezier_to(ctrl1.to_lyon(), ctrl2.to_lyon(), to.to_lyon());
                    }
                }
            }
            builder.end(contour.closed);
        }
        builder.build()
    }

    /// The silhouette's corners if the whole shape is one straight-edged closed
    /// contour with no interior detail, else `None`.
    ///
    /// This is exactly the condition for the polygon SDF: a polygon survives the
    /// non-uniform scale from unit box to item rect (its vertices simply move),
    /// whereas anything with a curve does not.
    pub fn as_polygon(&self) -> Option<Vec<Point>> {
        match (self.contours.as_slice(), self.details.is_empty()) {
            ([contour], true) if contour.closed => contour.as_polygon(),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::unit::p;

    fn square() -> Outline {
        Outline::polygon(&[p(0.0, 0.0), p(1.0, 0.0), p(1.0, 1.0), p(0.0, 1.0)])
    }

    #[test]
    fn clockwise_on_screen_is_positive_signed_area() {
        assert!(square().contours[0].signed_area() > 0.0);
        let anticlockwise =
            Contour::polygon(&[p(0.0, 0.0), p(0.0, 1.0), p(1.0, 1.0), p(1.0, 0.0)]);
        assert!(anticlockwise.signed_area() < 0.0);
    }

    #[test]
    fn curve_bounds_use_extrema_not_control_points() {
        // A quarter circle bulging right: the control polygon reaches x = 1.0 but
        // the curve only reaches x = 0.5523·(4/3·tan(22.5°))… well short of it.
        let mut b = Contour::builder(p(0.0, 0.0));
        let contour = b.arc(p(0.0, 1.0), 1.0, 1.0, -90.0, 0.0).close();
        let bounds = contour.bounds();
        assert!((bounds.max.x - 1.0).abs() < 1e-4, "{bounds:?}");
        assert!((bounds.min.y - 0.0).abs() < 1e-4, "{bounds:?}");
    }

    #[test]
    fn arcs_sweep_clockwise_on_screen() {
        // 0° is east and 90° is south, so a quarter sweep from 0° passes through
        // the bottom of the circle.
        let mut b = Contour::builder(p(1.0, 0.5));
        let contour = b.arc(p(0.5, 0.5), 0.5, 0.5, 0.0, 90.0).end_open();
        assert!(contour.segments.last().unwrap().end().distance(p(0.5, 1.0)) < 1e-5);
    }

    #[test]
    fn normalising_rescales_to_the_unit_box() {
        let half = Outline::polygon(&[p(0.25, 0.25), p(0.75, 0.25), p(0.75, 0.5)]).normalised();
        let bounds = half.bounds();
        assert!((bounds.min.x).abs() < 1e-6 && (bounds.min.y).abs() < 1e-6, "{bounds:?}");
        assert!((bounds.max.x - 1.0).abs() < 1e-6 && (bounds.max.y - 1.0).abs() < 1e-6);
    }

    #[test]
    fn normalising_an_already_unit_shape_changes_nothing() {
        assert_eq!(square().normalised(), square());
    }

    #[test]
    fn winding_is_zero_outside_and_non_zero_inside() {
        let s = square();
        assert!(s.contains(p(0.5, 0.5)));
        assert!(!s.contains(p(1.5, 0.5)));
        assert!(!s.contains(p(0.5, -0.5)));
    }

    /// Two overlapping contours wound the same way must read as one solid body,
    /// which is what lets a multi-document stack fill correctly.
    #[test]
    fn same_winding_contours_union_rather_than_cancel() {
        let overlapping = Outline::new(vec![
            Contour::polygon(&[p(0.0, 0.0), p(0.6, 0.0), p(0.6, 0.6), p(0.0, 0.6)]),
            Contour::polygon(&[p(0.4, 0.4), p(1.0, 0.4), p(1.0, 1.0), p(0.4, 1.0)]),
        ]);
        assert!(overlapping.contains(p(0.5, 0.5)), "the overlap is inside, not a hole");
        assert!(overlapping.contains(p(0.2, 0.2)));
        assert!(!overlapping.contains(p(0.9, 0.2)));
    }

    #[test]
    fn ray_exit_takes_the_far_wall_of_a_concave_shape() {
        // A rectangle with a slot cut up from the bottom edge, so a ray east at
        // y = 0.7 leaves the shape at x = 0.6, re-enters at 0.8, and finally leaves
        // at 1.0. An anchor belongs on the last of those, not the first.
        let slotted = Outline::polygon(&[
            p(0.0, 0.0),
            p(1.0, 0.0),
            p(1.0, 1.0),
            p(0.8, 1.0),
            p(0.8, 0.4),
            p(0.6, 0.4),
            p(0.6, 1.0),
            p(0.0, 1.0),
        ]);
        assert!(!slotted.contains(p(0.7, 0.7)), "the slot is outside the shape");
        let hit = slotted.ray_exit(p(0.1, 0.7), p(1.0, 0.0)).unwrap();
        assert!((hit.x - 1.0).abs() < 1e-5, "{hit:?}");
    }

    #[test]
    fn nearest_point_lands_on_an_edge() {
        let hit = square().nearest_point(p(0.5, -3.0));
        assert!(hit.distance(p(0.5, 0.0)) < 1e-6, "{hit:?}");
    }

    #[test]
    fn only_straight_single_contour_shapes_expose_a_polygon() {
        assert_eq!(square().as_polygon().unwrap().len(), 4);
        let mut b = Contour::builder(p(1.0, 0.5));
        let curved = Outline::from_contour(b.arc(p(0.5, 0.5), 0.5, 0.5, 0.0, 360.0).close());
        assert!(curved.as_polygon().is_none());
        let decorated = square().with_details(vec![Contour::line(p(0.1, 0.5), p(0.9, 0.5))]);
        assert!(decorated.as_polygon().is_none());
    }
}
