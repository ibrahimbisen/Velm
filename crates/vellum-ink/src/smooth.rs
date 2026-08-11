//! Turning raw pointer samples into a curve.
//!
//! A pointer reports position at a fixed rate, so the *spacing* of a stroke's points
//! encodes how fast the hand was moving, not how much detail the shape has. Drawing
//! the same arc slowly gives a hundred points a pixel apart; drawing it fast gives
//! eight points thirty pixels apart. Rendering either as a raw polyline is wrong in
//! opposite directions — the slow one is a smooth curve carrying visible sampling
//! jitter, the fast one is a visible chain of straight facets.
//!
//! # Why Catmull-Rom, and why centripetal
//!
//! Catmull-Rom **interpolates**: the output passes exactly through every input
//! point. That matters more for ink than for most curve work, because a stroke's
//! input points are not control handles someone chose — they are where the pen
//! actually was. An approximating spline (a B-spline, say) would shrink the loops of
//! handwriting and pull the ends inward, and the user would see their own writing
//! rendered slightly wrong with no way to say why.
//!
//! The parameterisation is **centripetal** (α = 0.5) rather than uniform (α = 0).
//! Uniform Catmull-Rom assumes equal spacing, and pointer input is the opposite of
//! equally spaced: when a long chord neighbours a short one — every time the hand
//! accelerates — uniform parameterisation overshoots into a cusp or a loop that was
//! never drawn. Centripetal parameterisation is provably free of cusps and
//! self-intersections *within* a segment, which is exactly the guarantee ink needs.
//! Chordal (α = 1) is also cusp-free but flattens curvature near sharp turns.
//!
//! # Adaptive resampling
//!
//! Each Catmull-Rom segment is converted to its cubic Bézier form, and the number of
//! line samples is derived from that cubic's own curvature rather than from a fixed
//! count. For a cubic, the polyline approximation error with `n` uniform samples is
//! bounded by `max|C''| / (8n²)`, and `max|C''|` for a Bézier is `6·max(|P₀−2P₁+P₂|,
//! |P₁−2P₂+P₃|)` — cheap and exact. Inverting the bound gives the sample count that
//! meets [`SmoothOptions::tolerance`].
//!
//! The consequence is the behaviour the problem actually calls for, with no special
//! cases: a densely sampled slow stroke has nearly flat segments, so `n` comes out as
//! 1 and no points are added at all; a sparse fast stroke has strongly curved
//! segments and gets subdivided until it is smooth. The same code, driven by the
//! geometry rather than by a guess about the input.
//!
//! # Corners
//!
//! An interpolating spline still *rounds* a sharp corner: it passes through the
//! vertex but arrives and leaves with a single shared tangent, so the point of a `V`
//! becomes an arc. Corners are therefore detected and used to cut the stroke into
//! spans that are splined independently, which gives the corner vertex two
//! independent tangents and keeps it sharp.
//!
//! Detection is a large turn angle **that is also a local maximum** among its
//! neighbouring turns. The threshold alone is not enough: a tight curve drawn fast
//! turns just as sharply per segment as a real corner does, and thresholding alone
//! would shatter it into facets. On a curve every vertex turns by roughly the same
//! amount, so no vertex is a local maximum and the curve survives; at a real corner
//! the turn spikes against near-straight neighbours and is caught.

use crate::stroke::{Stroke, StrokePoint};
use crate::vec2::Vec2;

/// Centripetal parameterisation. See the module docs for why this is not 0 or 1.
const ALPHA: f64 = 0.5;

/// Knot spacings below this are treated as degenerate and substituted. Only
/// reachable through a reflected phantom point at a span end, but the division it
/// guards would produce an infinite tangent.
const MIN_KNOT: f64 = 1e-12;

/// How the raw samples are turned into a curve.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SmoothOptions {
    /// Maximum distance, in world px, between the emitted polyline and the ideal
    /// spline it approximates.
    ///
    /// This bounds *flattening* error only. It says nothing about how far the spline
    /// departs from the input polyline — that departure is the smoothing, and it is
    /// intended.
    pub tolerance: f64,
    /// Turn angle, in degrees, above which a vertex may be treated as an intentional
    /// corner.
    ///
    /// 60° is chosen to sit above what a curve produces and below what a corner
    /// produces: a circle sampled at even twelve points turns only 30° per vertex,
    /// while the corner of a hand-drawn box or the point of a `V` turns 90° or more.
    /// The local-maximum test does the rest of the work.
    pub corner_angle_degrees: f64,
    /// Both chords meeting at a vertex must be at least this long, in world px, for
    /// it to count as a corner. A large turn between two sub-pixel chords is
    /// digitiser jitter, not a decision the hand made.
    pub corner_min_chord: f64,
    /// Upper bound on samples generated per input segment.
    ///
    /// A backstop, not a tuning knob: it bounds the work a single pathological
    /// segment can cause when the tolerance is driven to near zero by a deep zoom.
    pub max_samples_per_segment: u32,
}

impl Default for SmoothOptions {
    fn default() -> Self {
        Self {
            tolerance: 0.25,
            corner_angle_degrees: 60.0,
            corner_min_chord: 1.0,
            max_samples_per_segment: 32,
        }
    }
}

impl SmoothOptions {
    /// The defaults with a specific flattening tolerance — the form the LOD pipeline
    /// uses, since tolerance is the only field zoom should influence.
    pub fn with_tolerance(tolerance: f64) -> Self {
        Self { tolerance, ..Self::default() }
    }

    fn effective_tolerance(&self) -> f64 {
        // A zero or negative tolerance would demand infinite samples. Clamp to a
        // floor far below one screen pixel at any zoom Vellum permits.
        if self.tolerance.is_finite() { self.tolerance.max(1e-4) } else { 1e-4 }
    }
}

/// Interior vertices that should be treated as intentional corners.
///
/// Exposed because "where are this stroke's corners" is a question other tools need
/// — a shape-recognition or sharpening pass would ask the same thing — and because
/// it is the part of smoothing most worth testing directly.
pub fn corner_indices(points: &[StrokePoint], options: &SmoothOptions) -> Vec<usize> {
    if points.len() < 3 {
        return Vec::new();
    }

    let threshold = options.corner_angle_degrees.to_radians().abs();

    // turns[i] is the turn at points[i + 1]; the endpoints have no turn.
    let turns: Vec<f64> = points
        .windows(3)
        .map(|w| {
            let incoming = Vec2::from_point(w[1]) - Vec2::from_point(w[0]);
            let outgoing = Vec2::from_point(w[2]) - Vec2::from_point(w[1]);
            incoming.angle_to(outgoing).abs()
        })
        .collect();

    let mut corners = Vec::new();
    for (k, &turn) in turns.iter().enumerate() {
        let i = k + 1;
        if turn <= threshold {
            continue;
        }
        if points[i - 1].distance_to(points[i]) < options.corner_min_chord
            || points[i].distance_to(points[i + 1]) < options.corner_min_chord
        {
            continue;
        }
        // Strictly greater on one side and greater-or-equal on the other: with
        // strict comparisons a two-vertex plateau would yield no corner at all,
        // and with two loose comparisons a uniformly curving arc would yield every
        // vertex.
        let above_prev = k == 0 || turn > turns[k - 1];
        let above_next = k + 1 == turns.len() || turn >= turns[k + 1];
        if above_prev && above_next {
            corners.push(i);
        }
    }
    corners
}

/// Resamples a stroke onto its Catmull-Rom spline.
///
/// The output passes through every input point and adds samples only where curvature
/// requires them, so it is idempotent in the sense that matters: smoothing an
/// already-smooth stroke at the same tolerance adds nothing.
pub fn smooth(stroke: &Stroke, options: &SmoothOptions) -> Stroke {
    let points = stroke.points();
    // Zero, one or two points describe no curvature: a dot and a straight segment
    // are already exactly what the spline through them would be.
    if points.len() < 3 {
        return stroke.clone();
    }

    let corners = corner_indices(points, options);
    let tolerance = options.effective_tolerance();
    let max_samples = options.max_samples_per_segment.max(1) as usize;

    let mut out = Vec::with_capacity(points.len() * 2);
    out.push(points[0]);

    // Spans run corner-to-corner, sharing the corner vertex, so each span gets its
    // own tangents there and the vertex stays sharp.
    let mut span_start = 0usize;
    for &end in corners.iter().chain(core::iter::once(&(points.len() - 1))) {
        spline_span(&points[span_start..=end], tolerance, max_samples, &mut out);
        span_start = end;
    }

    stroke.with_points(out)
}

/// Splines one corner-free span, appending everything after its first point.
fn spline_span(
    span: &[StrokePoint],
    tolerance: f64,
    max_samples: usize,
    out: &mut Vec<StrokePoint>,
) {
    for i in 0..span.len().saturating_sub(1) {
        let p1 = Vec2::from_point(span[i]);
        let p2 = Vec2::from_point(span[i + 1]);
        // Reflected phantom control points at the span's ends. Reflection rather
        // than duplication: duplicating gives a zero-length knot interval and an
        // undefined tangent, while reflecting makes the tangent fall out as exactly
        // `p2 - p1`, which is the "leave straight, arrive straight" behaviour a
        // stroke's own ends should have anyway.
        let p0 = if i == 0 { p1 * 2.0 - p2 } else { Vec2::from_point(span[i - 1]) };
        let p3 =
            if i + 2 < span.len() { Vec2::from_point(span[i + 2]) } else { p2 * 2.0 - p1 };

        let (c1, c2) = catmull_rom_to_bezier(p0, p1, p2, p3);
        let n = sample_count(p1, c1, c2, p2, tolerance, max_samples);

        let (pressure_from, pressure_to) = (span[i].pressure, span[i + 1].pressure);
        for k in 1..n {
            let t = k as f64 / n as f64;
            let position = eval_cubic(p1, c1, c2, p2, t);
            out.push(position.to_point(pressure_from + (pressure_to - pressure_from) * t));
        }
        // Push the input point itself rather than the curve evaluated at t = 1, so
        // that "the spline interpolates its input" holds to the last bit rather
        // than to within rounding.
        out.push(span[i + 1]);
    }
}

/// The two inner control points of the cubic Bézier equal to the centripetal
/// Catmull-Rom segment from `p1` to `p2`.
///
/// Barry-Goldman's non-uniform formulation. With equal chord lengths it collapses to
/// the familiar uniform tangents `(p2 − p0)/2` and `(p3 − p1)/2`, which is the check
/// the unit tests pin.
fn catmull_rom_to_bezier(p0: Vec2, p1: Vec2, p2: Vec2, p3: Vec2) -> (Vec2, Vec2) {
    let t2 = (p2 - p1).length().powf(ALPHA);
    let mut t1 = (p1 - p0).length().powf(ALPHA);
    let mut t3 = (p3 - p2).length().powf(ALPHA);
    if t1 < MIN_KNOT {
        t1 = t2;
    }
    if t3 < MIN_KNOT {
        t3 = t2;
    }

    let chord = p2 - p1;
    let m1 = chord + ((p1 - p0) * (1.0 / t1) - (p2 - p0) * (1.0 / (t1 + t2))) * t2;
    let m2 = chord + ((p3 - p2) * (1.0 / t3) - (p3 - p1) * (1.0 / (t2 + t3))) * t2;

    (p1 + m1 * (1.0 / 3.0), p2 - m2 * (1.0 / 3.0))
}

/// Samples needed so the polyline stays within `tolerance` of the cubic.
///
/// From the standard bound `error ≤ max|C''| / (8n²)`, with `max|C''|` taken from
/// the control polygon's second differences.
fn sample_count(
    p0: Vec2,
    p1: Vec2,
    p2: Vec2,
    p3: Vec2,
    tolerance: f64,
    max_samples: usize,
) -> usize {
    let a = p0 - p1 * 2.0 + p2;
    let b = p1 - p2 * 2.0 + p3;
    let max_second_derivative = 6.0 * a.length().max(b.length());
    let n = (max_second_derivative / (8.0 * tolerance)).sqrt().ceil();
    if !n.is_finite() {
        return max_samples;
    }
    (n as usize).clamp(1, max_samples)
}

fn eval_cubic(p0: Vec2, p1: Vec2, p2: Vec2, p3: Vec2, t: f64) -> Vec2 {
    let s = 1.0 - t;
    p0 * (s * s * s) + p1 * (3.0 * s * s * t) + p2 * (3.0 * s * t * t) + p3 * (t * t * t)
}

impl Stroke {
    /// This stroke resampled onto its Catmull-Rom spline. See [`smooth`].
    pub fn smoothed(&self, options: &SmoothOptions) -> Stroke {
        smooth(self, options)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vec2::v;

    fn xy(points: &[(f64, f64)]) -> Stroke {
        Stroke::from_miro(points, Some(2.0))
    }

    /// A regular polygon approximating a circle, `n` vertices, radius `r`.
    fn circle(n: usize, r: f64) -> Stroke {
        let pts: Vec<_> = (0..n)
            .map(|i| {
                let a = std::f64::consts::TAU * i as f64 / n as f64;
                (r * a.cos(), r * a.sin())
            })
            .collect();
        xy(&pts)
    }

    #[test]
    fn degenerate_strokes_pass_through_untouched() {
        for pts in [&[][..], &[(0.0, 0.0)][..], &[(0.0, 0.0), (10.0, 0.0)][..]] {
            let s = xy(pts);
            assert_eq!(s.smoothed(&SmoothOptions::default()), s);
        }
    }

    /// The defining property of an interpolating spline, and the one users would
    /// notice losing: their own pen positions are all still on the curve.
    #[test]
    fn every_input_point_survives_in_order() {
        let s = xy(&[(0.0, 0.0), (10.0, 30.0), (40.0, 35.0), (70.0, 5.0), (90.0, 40.0)]);
        let out = s.smoothed(&SmoothOptions::with_tolerance(0.05));
        let mut remaining = out.points().iter().copied();
        for p in s.points() {
            assert!(
                remaining.any(|q| q == *p),
                "input point {p:?} missing from the smoothed stroke"
            );
        }
    }

    /// Collinear input has zero curvature, so the adaptive rule must add nothing —
    /// this is what keeps a straight ruled line from costing a hundred vertices.
    #[test]
    fn collinear_points_gain_no_samples() {
        let s = xy(&[(0.0, 0.0), (10.0, 0.0), (20.0, 0.0), (30.0, 0.0), (40.0, 0.0)]);
        let out = s.smoothed(&SmoothOptions::with_tolerance(0.01));
        assert_eq!(out.len(), s.len());
    }

    /// The headline behaviour: sparse input is subdivided, dense input is not.
    /// Both strokes trace the same arc, so any difference is the sampling rate.
    #[test]
    fn a_sparse_stroke_is_subdivided_far_more_than_a_dense_one() {
        let sparse = circle(8, 100.0);
        let dense = circle(96, 100.0);
        let options = SmoothOptions::with_tolerance(0.25);

        let sparse_growth = sparse.smoothed(&options).len() as f64 / sparse.len() as f64;
        let dense_growth = dense.smoothed(&options).len() as f64 / dense.len() as f64;

        assert!(sparse_growth > 4.0, "sparse growth {sparse_growth}");
        assert!(dense_growth < 1.6, "dense growth {dense_growth}");
    }

    /// The flattening bound is the whole justification for the sample count, so it
    /// is checked against the curve it claims to approximate: every emitted point
    /// must lie within tolerance of the polyline through its neighbours' true
    /// positions. Sampling the same arc far more finely and measuring the gap is the
    /// most direct available test.
    #[test]
    fn output_stays_within_tolerance_of_a_much_finer_sampling() {
        let coarse = circle(10, 200.0);
        for tolerance in [1.0, 0.25, 0.05] {
            let smoothed = coarse.smoothed(&SmoothOptions::with_tolerance(tolerance));
            let reference = coarse.smoothed(&SmoothOptions::with_tolerance(tolerance / 64.0));
            let worst = reference
                .points()
                .iter()
                .map(|p| crate::hit::distance_to_polyline((p.x, p.y), smoothed.points()).0)
                .fold(0.0_f64, f64::max);
            assert!(worst <= tolerance * 1.5, "tolerance {tolerance}: worst gap {worst}");
        }
    }

    #[test]
    fn a_sampled_circle_has_no_corners() {
        for n in [8, 12, 24, 64] {
            let c = circle(n, 100.0);
            assert!(
                corner_indices(c.points(), &SmoothOptions::default()).is_empty(),
                "a {n}-gon should read as a curve, not as {n} corners"
            );
        }
    }

    #[test]
    fn a_right_angle_is_a_corner() {
        let s = xy(&[(0.0, 0.0), (30.0, 0.0), (60.0, 0.0), (60.0, 30.0), (60.0, 60.0)]);
        assert_eq!(corner_indices(s.points(), &SmoothOptions::default()), vec![2]);
    }

    /// Without the chord-length guard, digitiser jitter one pixel wide would be
    /// promoted to a corner and the curve would be cut into fragments.
    #[test]
    fn sub_pixel_jitter_is_not_a_corner() {
        let s = xy(&[
            (0.0, 0.0),
            (0.3, 0.02),
            (0.6, -0.02),
            (0.9, 0.02),
            (1.2, -0.02),
            (1.5, 0.0),
        ]);
        assert!(corner_indices(s.points(), &SmoothOptions::default()).is_empty());
    }

    /// The point of splitting spans at corners: the vertex keeps its angle instead
    /// of being smoothed into an arc. Measured by how far the curve strays from the
    /// two straight legs near the corner.
    #[test]
    fn a_corner_is_not_rounded_off() {
        let s = xy(&[
            (0.0, 0.0),
            (40.0, 0.0),
            (80.0, 0.0),
            (80.0, 40.0),
            (80.0, 80.0),
        ]);
        let out = s.smoothed(&SmoothOptions::with_tolerance(0.05));
        for p in out.points() {
            let on_horizontal = p.y.abs() < 1e-9 && (0.0..=80.0).contains(&p.x);
            let on_vertical = (p.x - 80.0).abs() < 1e-9 && (0.0..=80.0).contains(&p.y);
            assert!(on_horizontal || on_vertical, "corner was rounded: {p:?}");
        }
    }

    #[test]
    fn pressure_is_interpolated_across_added_samples() {
        let s = Stroke::new(
            [
                StrokePoint::with_pressure(0.0, 0.0, 0.2),
                StrokePoint::with_pressure(50.0, 20.0, 0.6),
                StrokePoint::with_pressure(100.0, 0.0, 1.0),
            ],
            4.0,
        );
        let out = s.smoothed(&SmoothOptions::with_tolerance(0.05));
        assert!(out.len() > 3);
        assert_eq!(out.points().first().unwrap().pressure, 0.2);
        assert_eq!(out.points().last().unwrap().pressure, 1.0);
        assert!(
            out.points().windows(2).all(|w| w[0].pressure <= w[1].pressure + 1e-12),
            "pressure should ramp monotonically between monotonic samples"
        );
    }

    /// A stroke that crosses itself is ordinary handwriting (any lowercase `e`), and
    /// centripetal parameterisation must not turn the crossing into a cusp.
    #[test]
    fn a_self_intersecting_stroke_smooths_without_blowing_up() {
        let s = xy(&[
            (0.0, 0.0),
            (40.0, 40.0),
            (80.0, 0.0),
            (40.0, -40.0),
            (20.0, 20.0),
            (60.0, 20.0),
        ]);
        let out = s.smoothed(&SmoothOptions::with_tolerance(0.1));
        assert!(out.points().iter().all(|p| p.x.is_finite() && p.y.is_finite()));
        // Overshoot beyond the input's own extent is the cusp symptom.
        assert!(out.points().iter().all(|p| p.x > -40.0 && p.x < 120.0));
    }

    #[test]
    fn the_uniform_case_reproduces_the_classic_tangents() {
        // Equal chord lengths, so the non-uniform formula must collapse to the
        // uniform one: control points at p1 + (p2 - p0)/6 and p2 - (p3 - p1)/6.
        let (p0, p1, p2, p3) = (v(0.0, 0.0), v(1.0, 0.0), v(2.0, 0.0), v(3.0, 0.0));
        let (c1, c2) = catmull_rom_to_bezier(p0, p1, p2, p3);
        assert!((c1.x - (1.0 + 2.0 / 6.0)).abs() < 1e-12, "{c1:?}");
        assert!((c2.x - (2.0 - 2.0 / 6.0)).abs() < 1e-12, "{c2:?}");
    }

    #[test]
    fn a_finer_tolerance_never_produces_fewer_samples() {
        let s = circle(9, 150.0);
        let mut previous = 0;
        for tolerance in [4.0, 1.0, 0.25, 0.05, 0.01] {
            let n = s.smoothed(&SmoothOptions::with_tolerance(tolerance)).len();
            assert!(n >= previous, "tolerance {tolerance} gave {n} after {previous}");
            previous = n;
        }
    }

    #[test]
    fn the_sample_cap_bounds_a_pathological_segment() {
        let options = SmoothOptions {
            tolerance: 1e-9,
            max_samples_per_segment: 4,
            ..SmoothOptions::default()
        };
        let s = xy(&[(0.0, 0.0), (500.0, 900.0), (1000.0, 0.0)]);
        // Two segments, at most four samples each, plus the shared endpoints.
        assert!(s.smoothed(&options).len() <= 2 * 4 + 1);
    }
}
