//! Board-space geometry, in `f64` with y pointing down.
//!
//! Two decisions are worth recording.
//!
//! **`f64`, not `f32`.** The renderer works in camera-relative `f32` because that
//! is what a GPU consumes (`docs/01-architecture.md` §3), but an exporter has no
//! camera to rebase against: it writes absolute board coordinates, and the
//! reference board is already 41,282 × 17,515 px. At `f32`, coordinates that large
//! quantise to about 4 milli-pixels, which is visible as jitter once an export is
//! zoomed into. `f64` costs nothing here — export is not a per-frame path.
//!
//! **y points down**, matching the canvas, Miro's SVG export and SVG itself. The
//! PDF writer is the only place that flips, and it does so by transforming
//! coordinates explicitly rather than by a mirroring CTM, so nothing downstream of
//! it has to remember that text would come out backwards.

use vellum_shapes::outline::{Contour, Outline, Segment as UnitSegment};
use vellum_shapes::unit::Point as UnitPoint;

/// A point in board coordinates.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

/// Shorthand for [`Point::new`], to keep geometry literals readable.
pub const fn pt(x: f64, y: f64) -> Point {
    Point { x, y }
}

impl Point {
    pub const fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }

    pub fn distance(self, other: Self) -> f64 {
        ((self.x - other.x).powi(2) + (self.y - other.y).powi(2)).sqrt()
    }
}

/// An axis-aligned rectangle, `width` and `height` non-negative by construction of
/// every constructor here.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl Rect {
    pub const fn new(x: f64, y: f64, width: f64, height: f64) -> Self {
        Self { x, y, width, height }
    }

    /// The rectangle spanned by two corners, in either order.
    pub fn from_corners(a: Point, b: Point) -> Self {
        Self {
            x: a.x.min(b.x),
            y: a.y.min(b.y),
            width: (a.x - b.x).abs(),
            height: (a.y - b.y).abs(),
        }
    }

    /// The tight box around a set of points, or `None` if there are none.
    pub fn of_points(points: impl IntoIterator<Item = Point>) -> Option<Self> {
        let mut iter = points.into_iter();
        let first = iter.next()?;
        let (mut min, mut max) = (first, first);
        for p in iter {
            min = pt(min.x.min(p.x), min.y.min(p.y));
            max = pt(max.x.max(p.x), max.y.max(p.y));
        }
        Some(Self::from_corners(min, max))
    }

    pub fn right(&self) -> f64 {
        self.x + self.width
    }

    pub fn bottom(&self) -> f64 {
        self.y + self.height
    }

    pub fn centre(&self) -> Point {
        pt(self.x + self.width / 2.0, self.y + self.height / 2.0)
    }

    pub fn corners(&self) -> [Point; 4] {
        [
            pt(self.x, self.y),
            pt(self.right(), self.y),
            pt(self.right(), self.bottom()),
            pt(self.x, self.bottom()),
        ]
    }

    /// True when the rectangle encloses no area, and therefore nothing can be seen
    /// through it. Used as the "clipped away entirely" test.
    pub fn is_empty(&self) -> bool {
        !(self.width > 0.0 && self.height > 0.0)
    }

    pub fn union(self, other: Self) -> Self {
        Self::from_corners(
            pt(self.x.min(other.x), self.y.min(other.y)),
            pt(self.right().max(other.right()), self.bottom().max(other.bottom())),
        )
    }

    /// The overlap, or `None` when the rectangles do not meet. Touching edges count
    /// as no overlap: a zero-area intersection renders nothing.
    pub fn intersection(self, other: Self) -> Option<Self> {
        let x = self.x.max(other.x);
        let y = self.y.max(other.y);
        let right = self.right().min(other.right());
        let bottom = self.bottom().min(other.bottom());
        (right > x && bottom > y).then(|| Self::new(x, y, right - x, bottom - y))
    }

    /// True when `other` lies entirely within this rectangle — the test for "this
    /// clip removes nothing", which is worth making because a clip that removes
    /// nothing can be skipped entirely.
    pub fn contains_rect(&self, other: &Self) -> bool {
        other.x >= self.x
            && other.y >= self.y
            && other.right() <= self.right()
            && other.bottom() <= self.bottom()
    }

    /// Grows the rectangle by `amount` on every side. A negative amount shrinks it
    /// and is clamped at zero size rather than inverting.
    pub fn inflated(self, amount: f64) -> Self {
        let width = (self.width + amount * 2.0).max(0.0);
        let height = (self.height + amount * 2.0).max(0.0);
        Self {
            x: self.x + (self.width - width) / 2.0,
            y: self.y + (self.height - height) / 2.0,
            width,
            height,
        }
    }

    /// Maps the unit box `0..1 × 0..1` onto this rectangle.
    pub fn from_unit(&self, u: Point) -> Point {
        pt(self.x + u.x * self.width, self.y + u.y * self.height)
    }
}

/// A 2×3 affine transform, laid out as SVG's `matrix(a b c d e f)`:
/// `x' = a·x + c·y + e`, `y' = b·x + d·y + f`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Affine {
    pub a: f64,
    pub b: f64,
    pub c: f64,
    pub d: f64,
    pub e: f64,
    pub f: f64,
}

impl Default for Affine {
    fn default() -> Self {
        Self::IDENTITY
    }
}

impl Affine {
    pub const IDENTITY: Self = Self { a: 1.0, b: 0.0, c: 0.0, d: 1.0, e: 0.0, f: 0.0 };

    pub const fn translation(dx: f64, dy: f64) -> Self {
        Self { a: 1.0, b: 0.0, c: 0.0, d: 1.0, e: dx, f: dy }
    }

    pub const fn scaling(sx: f64, sy: f64) -> Self {
        Self { a: sx, b: 0.0, c: 0.0, d: sy, e: 0.0, f: 0.0 }
    }

    /// Rotation by `degrees` **clockwise on screen** about `centre`, which is what
    /// a positive rotation means once y points down — and what Miro stores.
    pub fn rotation_about(degrees: f64, centre: Point) -> Self {
        let (sin, cos) = degrees.to_radians().sin_cos();
        Self {
            a: cos,
            b: sin,
            c: -sin,
            d: cos,
            e: centre.x - cos * centre.x + sin * centre.y,
            f: centre.y - sin * centre.x - cos * centre.y,
        }
    }

    /// `self` first, then `next`.
    pub fn then(self, next: Self) -> Self {
        Self {
            a: next.a * self.a + next.c * self.b,
            b: next.b * self.a + next.d * self.b,
            c: next.a * self.c + next.c * self.d,
            d: next.b * self.c + next.d * self.d,
            e: next.a * self.e + next.c * self.f + next.e,
            f: next.b * self.e + next.d * self.f + next.f,
        }
    }

    pub fn apply(&self, p: Point) -> Point {
        pt(self.a * p.x + self.c * p.y + self.e, self.b * p.x + self.d * p.y + self.f)
    }

    pub fn is_identity(&self) -> bool {
        *self == Self::IDENTITY
    }
}

/// One segment of a sub-path, always continuing from the previous end point.
///
/// Mirrors [`vellum_shapes::outline::Segment`] in `f64` board space. The duplication
/// is deliberate: this crate must describe ink strokes and connector routes that
/// never passed through the shape catalogue, and an exporter that could only speak
/// about catalogue shapes would be useless for two thirds of a real board.
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

    fn transformed(&self, t: &Affine) -> Self {
        match *self {
            Self::Line { to } => Self::Line { to: t.apply(to) },
            Self::Quadratic { ctrl, to } => {
                Self::Quadratic { ctrl: t.apply(ctrl), to: t.apply(to) }
            }
            Self::Cubic { ctrl1, ctrl2, to } => Self::Cubic {
                ctrl1: t.apply(ctrl1),
                ctrl2: t.apply(ctrl2),
                to: t.apply(to),
            },
        }
    }

    /// The same curve as a cubic. PDF has no quadratic operator, so every
    /// quadratic is raised before it is written; the standard elevation is exact,
    /// not an approximation.
    pub fn to_cubic(&self, from: Point) -> (Point, Point, Point) {
        match *self {
            Self::Line { to } => (from, to, to),
            Self::Quadratic { ctrl, to } => (
                pt(from.x + 2.0 / 3.0 * (ctrl.x - from.x), from.y + 2.0 / 3.0 * (ctrl.y - from.y)),
                pt(to.x + 2.0 / 3.0 * (ctrl.x - to.x), to.y + 2.0 / 3.0 * (ctrl.y - to.y)),
                to,
            ),
            Self::Cubic { ctrl1, ctrl2, to } => (ctrl1, ctrl2, to),
        }
    }

    /// Exact bounds of the segment, curve extrema included.
    ///
    /// Control points are *not* used as the box: a cubic quarter-circle's control
    /// polygon overshoots the curve by about 10% of the radius, and a shape whose
    /// export bounds were 10% loose would sit visibly off-centre on its page.
    fn bounds(&self, from: Point) -> Rect {
        match *self {
            Self::Line { to } => Rect::from_corners(from, to),
            Self::Quadratic { ctrl, to } => {
                let axis = |p0: f64, p1: f64, p2: f64| {
                    let denom = p0 - 2.0 * p1 + p2;
                    let mut lo = p0.min(p2);
                    let mut hi = p0.max(p2);
                    if denom.abs() > f64::EPSILON {
                        let t = (p0 - p1) / denom;
                        if (0.0..=1.0).contains(&t) {
                            let v = (1.0 - t).powi(2) * p0 + 2.0 * (1.0 - t) * t * p1 + t * t * p2;
                            lo = lo.min(v);
                            hi = hi.max(v);
                        }
                    }
                    (lo, hi)
                };
                let (x0, x1) = axis(from.x, ctrl.x, to.x);
                let (y0, y1) = axis(from.y, ctrl.y, to.y);
                Rect::from_corners(pt(x0, y0), pt(x1, y1))
            }
            Self::Cubic { ctrl1, ctrl2, to } => {
                let axis = |p0: f64, p1: f64, p2: f64, p3: f64| {
                    let mut lo = p0.min(p3);
                    let mut hi = p0.max(p3);
                    // B'(t) = 0 is a quadratic in t; both roots that fall inside
                    // 0..1 are extrema of this axis.
                    let a = -p0 + 3.0 * p1 - 3.0 * p2 + p3;
                    let b = 2.0 * (p0 - 2.0 * p1 + p2);
                    let c = p1 - p0;
                    let mut visit = |t: f64| {
                        if (0.0..=1.0).contains(&t) {
                            let s = 1.0 - t;
                            let v = s * s * s * p0
                                + 3.0 * s * s * t * p1
                                + 3.0 * s * t * t * p2
                                + t * t * t * p3;
                            lo = lo.min(v);
                            hi = hi.max(v);
                        }
                    };
                    if a.abs() < 1e-12 {
                        if b.abs() > 1e-12 {
                            visit(-c / b);
                        }
                    } else {
                        let disc = b * b - 4.0 * a * c;
                        if disc >= 0.0 {
                            let root = disc.sqrt();
                            visit((-b + root) / (2.0 * a));
                            visit((-b - root) / (2.0 * a));
                        }
                    }
                    (lo, hi)
                };
                let (x0, x1) = axis(from.x, ctrl1.x, ctrl2.x, to.x);
                let (y0, y1) = axis(from.y, ctrl1.y, ctrl2.y, to.y);
                Rect::from_corners(pt(x0, y0), pt(x1, y1))
            }
        }
    }
}

/// A single connected run of segments.
#[derive(Debug, Clone, PartialEq)]
pub struct SubPath {
    pub start: Point,
    pub segments: Vec<Segment>,
    /// A closed sub-path has an implicit final edge back to `start`. Fills always
    /// behave as if closed; only the *stroke* differs, which is why an ink stroke
    /// and a shape outline cannot share one flag.
    pub closed: bool,
}

impl SubPath {
    pub fn open(start: Point, segments: Vec<Segment>) -> Self {
        Self { start, segments, closed: false }
    }

    pub fn closed(start: Point, segments: Vec<Segment>) -> Self {
        Self { start, segments, closed: true }
    }

    /// A polyline through `points`, which is what ink and simple connector routes
    /// reduce to. Returns `None` for an empty slice.
    pub fn polyline(points: &[Point], closed: bool) -> Option<Self> {
        let (start, rest) = points.split_first()?;
        Some(Self {
            start: *start,
            segments: rest.iter().map(|&to| Segment::Line { to }).collect(),
            closed,
        })
    }

    pub fn bounds(&self) -> Rect {
        let mut bounds = Rect::from_corners(self.start, self.start);
        let mut cursor = self.start;
        for segment in &self.segments {
            bounds = bounds.union(segment.bounds(cursor));
            cursor = segment.end();
        }
        bounds
    }
}

/// A path in absolute board coordinates.
///
/// This is the exporter's universal geometry: catalogue shapes arrive through
/// [`Path::from_outline`], ink strokes and connector routes are built directly, and
/// every writer consumes only this.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Path {
    pub subpaths: Vec<SubPath>,
}

impl Path {
    pub fn new(subpaths: Vec<SubPath>) -> Self {
        Self { subpaths }
    }

    pub fn is_empty(&self) -> bool {
        self.subpaths.iter().all(|s| s.segments.is_empty())
    }

    /// Bounds of the geometry alone — no stroke width. Callers that need the inked
    /// extent add half the stroke width themselves, because the exporter does not
    /// know whether a given writer strokes centred or inside.
    pub fn bounds(&self) -> Option<Rect> {
        self.subpaths.iter().map(SubPath::bounds).reduce(Rect::union)
    }

    /// The path's corners when it is one closed, straight-edged sub-path, else
    /// `None`.
    ///
    /// Mirrors [`vellum_shapes::outline::Outline::as_polygon`], and is used for the
    /// same reason in reverse: a path that is really a polygon should be *written*
    /// as one. See [`crate::svg`] — `<polygon>` is shorter than the equivalent `d`
    /// attribute, and short matters, because Miro's SVG reader distinguishes ink
    /// from decoration by path length.
    pub fn as_polygon(&self) -> Option<Vec<Point>> {
        let [sub] = self.subpaths.as_slice() else { return None };
        if !sub.closed || sub.segments.is_empty() {
            return None;
        }
        sub.segments
            .iter()
            .all(|s| matches!(s, Segment::Line { .. }))
            .then(|| std::iter::once(sub.start).chain(sub.segments.iter().map(Segment::end)).collect())
    }

    pub fn transformed(&self, t: &Affine) -> Self {
        if t.is_identity() {
            return self.clone();
        }
        Self {
            subpaths: self
                .subpaths
                .iter()
                .map(|s| SubPath {
                    start: t.apply(s.start),
                    segments: s.segments.iter().map(|seg| seg.transformed(t)).collect(),
                    closed: s.closed,
                })
                .collect(),
        }
    }

    /// Maps a [`vellum_shapes`] outline out of its unit box and onto `rect`.
    ///
    /// This is the whole bridge between the shape catalogue and the exporter, and
    /// it is a plain non-uniform scale because that is exactly what the unit box was
    /// designed for: `vellum-shapes` normalises every silhouette to fill `0..1`, so
    /// mapping it onto an item rect reproduces the shape at the size the user drew
    /// it. Interior detail contours (the bars of a predefined process, the rim of a
    /// cylinder) come through as open sub-paths, so a writer that strokes without
    /// filling them gets the flowchart symbols right.
    pub fn from_outline(outline: &Outline, rect: Rect) -> Self {
        let map = |u: UnitPoint| rect.from_unit(pt(u.x as f64, u.y as f64));
        let convert = |c: &Contour| SubPath {
            start: map(c.start),
            segments: c
                .segments
                .iter()
                .map(|s| match *s {
                    UnitSegment::Line { to } => Segment::Line { to: map(to) },
                    UnitSegment::Quadratic { ctrl, to } => {
                        Segment::Quadratic { ctrl: map(ctrl), to: map(to) }
                    }
                    UnitSegment::Cubic { ctrl1, ctrl2, to } => Segment::Cubic {
                        ctrl1: map(ctrl1),
                        ctrl2: map(ctrl2),
                        to: map(to),
                    },
                })
                .collect(),
            closed: c.closed,
        };
        Self {
            subpaths: outline.contours.iter().chain(&outline.details).map(convert).collect(),
        }
    }
}

/// One end of an open path, with the direction an end cap should point.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Terminal {
    pub point: Point,
    /// Degrees clockwise from east, pointing **away** from the path — which is the
    /// way an arrowhead faces at either end.
    pub direction_deg: f64,
}

impl Path {
    /// The two ends of the path, with outward directions for end caps.
    ///
    /// Direction comes from the nearest *distinct* neighbouring point, control
    /// points included: a cubic's handle is what sets the tangent at its endpoint,
    /// and using the far endpoint instead would swing an arrowhead off the curve.
    /// Degenerate paths — one point, or every point coincident — return `None`
    /// rather than an arbitrary angle.
    pub fn terminals(&self) -> Option<(Terminal, Terminal)> {
        let first = self.subpaths.iter().find(|s| !s.segments.is_empty())?;
        let last = self.subpaths.iter().rev().find(|s| !s.segments.is_empty())?;

        // Forwards from the start: the first control point that is not the start.
        let start = first.start;
        let after = match first.segments[0] {
            Segment::Line { to } => to,
            Segment::Quadratic { ctrl, to } => {
                if ctrl.distance(start) > 1e-9 {
                    ctrl
                } else {
                    to
                }
            }
            Segment::Cubic { ctrl1, ctrl2, to } => {
                if ctrl1.distance(start) > 1e-9 {
                    ctrl1
                } else if ctrl2.distance(start) > 1e-9 {
                    ctrl2
                } else {
                    to
                }
            }
        };

        // Backwards from the end: the last control point that is not the end.
        let tail = last.segments.last().expect("filtered on non-empty");
        let end = tail.end();
        let previous = last
            .segments
            .len()
            .checked_sub(2)
            .map_or(last.start, |i| last.segments[i].end());
        let before = match *tail {
            Segment::Line { .. } => previous,
            Segment::Quadratic { ctrl, .. } => {
                if ctrl.distance(end) > 1e-9 {
                    ctrl
                } else {
                    previous
                }
            }
            Segment::Cubic { ctrl1, ctrl2, .. } => {
                if ctrl2.distance(end) > 1e-9 {
                    ctrl2
                } else if ctrl1.distance(end) > 1e-9 {
                    ctrl1
                } else {
                    previous
                }
            }
        };

        let angle = |from: Point, to: Point| (to.y - from.y).atan2(to.x - from.x).to_degrees();
        (start.distance(after) > 1e-9 && end.distance(before) > 1e-9).then(|| {
            (
                Terminal { point: start, direction_deg: angle(after, start) },
                Terminal { point: end, direction_deg: angle(before, end) },
            )
        })
    }
}

/// Formats a coordinate for a text format, trimmed to `precision` decimals.
///
/// Trailing zeros are stripped and `-0` is normalised to `0`. Both matter more than
/// they look: coordinate text is the bulk of an SVG's bytes — Miro's export of the
/// reference board is 36MB — and `-0` is a legal but unhelpful thing to hand a
/// downstream parser.
pub(crate) fn write_num(out: &mut String, value: f64, precision: u8) {
    if !value.is_finite() {
        out.push('0');
        return;
    }
    let precision = precision as usize;
    let text = format!("{value:.precision$}");
    let trimmed =
        if text.contains('.') { text.trim_end_matches('0').trim_end_matches('.') } else { &text };
    match trimmed {
        "" | "-0" | "-" => out.push('0'),
        other => out.push_str(other),
    }
}

pub(crate) fn num(value: f64, precision: u8) -> String {
    let mut out = String::new();
    write_num(&mut out, value, precision);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use vellum_shapes::Shape;

    #[test]
    fn rect_from_corners_normalises_either_order() {
        let a = Rect::from_corners(pt(10.0, 20.0), pt(0.0, 0.0));
        assert_eq!(a, Rect::new(0.0, 0.0, 10.0, 20.0));
        assert_eq!(a, Rect::from_corners(pt(0.0, 0.0), pt(10.0, 20.0)));
    }

    #[test]
    fn touching_rectangles_do_not_intersect() {
        let left = Rect::new(0.0, 0.0, 10.0, 10.0);
        let right = Rect::new(10.0, 0.0, 10.0, 10.0);
        assert!(left.intersection(right).is_none(), "a zero-area overlap shows nothing");
        assert_eq!(
            left.intersection(Rect::new(5.0, 5.0, 10.0, 10.0)),
            Some(Rect::new(5.0, 5.0, 5.0, 5.0))
        );
    }

    #[test]
    fn inflating_past_zero_clamps_rather_than_inverting() {
        let r = Rect::new(0.0, 0.0, 4.0, 4.0).inflated(-10.0);
        assert!(r.is_empty());
        assert!(r.width >= 0.0 && r.height >= 0.0);
    }

    #[test]
    fn rotation_is_clockwise_on_screen() {
        // y down, so +90° takes the +x axis onto the +y axis.
        let t = Affine::rotation_about(90.0, pt(0.0, 0.0));
        let p = t.apply(pt(1.0, 0.0));
        assert!(p.x.abs() < 1e-12 && (p.y - 1.0).abs() < 1e-12, "{p:?}");
    }

    #[test]
    fn rotation_about_a_centre_leaves_the_centre_alone() {
        let centre = pt(37.0, -12.5);
        let moved = Affine::rotation_about(41.0, centre).apply(centre);
        assert!(moved.distance(centre) < 1e-9, "{moved:?}");
    }

    #[test]
    fn composition_applies_self_before_next() {
        let t = Affine::translation(10.0, 0.0).then(Affine::scaling(2.0, 2.0));
        assert_eq!(t.apply(pt(0.0, 0.0)), pt(20.0, 0.0), "translate first, then scale");
    }

    /// A cubic quarter circle: the control polygon reaches x = 1 but the curve
    /// stops well short, so control-point bounds would be 10% too wide.
    #[test]
    fn curve_bounds_use_extrema_not_control_points() {
        let k = 0.552_284_75;
        let arc = SubPath::open(
            pt(0.0, 0.0),
            vec![Segment::Cubic { ctrl1: pt(k, 0.0), ctrl2: pt(1.0, 1.0 - k), to: pt(1.0, 1.0) }],
        );
        let b = arc.bounds();
        assert!((b.width - 1.0).abs() < 1e-9 && (b.height - 1.0).abs() < 1e-9, "{b:?}");

        // And a curve that genuinely overshoots its endpoints is still captured.
        let bulge = SubPath::open(
            pt(0.0, 0.0),
            vec![Segment::Quadratic { ctrl: pt(0.5, 4.0), to: pt(1.0, 0.0) }],
        );
        assert!((bulge.bounds().height - 2.0).abs() < 1e-9, "{:?}", bulge.bounds());
    }

    #[test]
    fn quadratics_raise_to_cubics_exactly() {
        let from = pt(0.0, 0.0);
        let seg = Segment::Quadratic { ctrl: pt(1.0, 2.0), to: pt(2.0, 0.0) };
        let (c1, c2, to) = seg.to_cubic(from);
        // Evaluate both at t = 0.5 and compare.
        let quad = pt(0.25 * from.x + 0.5 * 1.0 + 0.25 * 2.0, 0.25 * from.y + 0.5 * 2.0 + 0.0);
        let cubic = pt(
            0.125 * from.x + 0.375 * c1.x + 0.375 * c2.x + 0.125 * to.x,
            0.125 * from.y + 0.375 * c1.y + 0.375 * c2.y + 0.125 * to.y,
        );
        assert!(quad.distance(cubic) < 1e-12, "{quad:?} vs {cubic:?}");
    }

    /// Every catalogue shape must land exactly on the rect it was asked for —
    /// this is what makes an exported shape line up with its selection handles.
    #[test]
    fn every_catalogue_shape_fills_its_rect() {
        let rect = Rect::new(100.0, 200.0, 300.0, 150.0);
        for shape in vellum_shapes::CATALOGUE {
            let outline = shape.outline(rect.width as f32 / rect.height as f32);
            let path = Path::from_outline(&outline, rect);
            let bounds = path.bounds().expect("a shape has geometry");
            // Detail contours live inside the silhouette, so the union of both is
            // still the silhouette's box.
            assert!(
                (bounds.x - rect.x).abs() < 0.05
                    && (bounds.y - rect.y).abs() < 0.05
                    && (bounds.width - rect.width).abs() < 0.05
                    && (bounds.height - rect.height).abs() < 0.05,
                "{} exported to {bounds:?}, expected {rect:?}",
                shape.name()
            );
        }
    }

    #[test]
    fn shape_details_survive_the_mapping() {
        // A predefined process has two interior bars; they must reach the export
        // or the flowchart symbol is just a rectangle.
        let shape = Shape::predefined_process();
        let outline = shape.outline(1.0);
        assert!(!outline.details.is_empty(), "fixture assumption");
        let path = Path::from_outline(&outline, Rect::new(0.0, 0.0, 10.0, 10.0));
        assert_eq!(path.subpaths.len(), outline.contours.len() + outline.details.len());
    }

    #[test]
    fn terminals_point_outward_along_the_path() {
        let line = Path::new(vec![
            SubPath::polyline(&[pt(0.0, 0.0), pt(10.0, 0.0)], false).unwrap(),
        ]);
        let (start, end) = line.terminals().unwrap();
        assert_eq!(start.point, pt(0.0, 0.0));
        assert!((start.direction_deg - 180.0).abs() < 1e-9, "{start:?}");
        assert_eq!(end.point, pt(10.0, 0.0));
        assert!(end.direction_deg.abs() < 1e-9, "{end:?}");
    }

    /// A curve's tangent comes from its control handle, not from the far endpoint —
    /// otherwise an arrowhead on a curved connector points across the curve.
    #[test]
    fn a_curves_terminal_follows_its_control_handle() {
        let curve = Path::new(vec![SubPath::open(
            pt(0.0, 0.0),
            vec![Segment::Cubic { ctrl1: pt(0.0, -10.0), ctrl2: pt(10.0, 10.0), to: pt(10.0, 0.0) }],
        )]);
        let (start, end) = curve.terminals().unwrap();
        assert!((start.direction_deg - 90.0).abs() < 1e-9, "{start:?}");
        assert!((end.direction_deg + 90.0).abs() < 1e-9, "{end:?}");
    }

    #[test]
    fn a_degenerate_path_has_no_terminal_direction() {
        assert!(Path::default().terminals().is_none());
        let dot = Path::new(vec![
            SubPath::polyline(&[pt(3.0, 3.0), pt(3.0, 3.0)], false).unwrap(),
        ]);
        assert!(dot.terminals().is_none(), "a zero-length stroke has no direction");
    }

    #[test]
    fn numbers_are_trimmed_and_never_negative_zero() {
        assert_eq!(num(1.5, 2), "1.5");
        assert_eq!(num(1.0, 2), "1");
        assert_eq!(num(-0.001, 2), "0");
        assert_eq!(num(1.005_1, 2), "1.01");
        assert_eq!(num(f64::NAN, 2), "0");
        assert_eq!(num(-12.25, 3), "-12.25");
    }
}
