//! The stroke itself — a sanitised point list that every other module in the crate
//! is allowed to trust.
//!
//! Sanitisation happens once, in [`Stroke::new`], and it is the crate's entire
//! defence against a NaN reaching a GPU vertex buffer. A single non-finite
//! coordinate anywhere upstream poisons every segment normal that touches it, and
//! the symptom is not a crash but a triangle stretched across the whole viewport —
//! expensive to diagnose from a screenshot. Two consecutive identical points are
//! just as bad: the segment direction is `0/0`, so a duplicate that Miro considers
//! harmless becomes a NaN normal. Both are removed at the boundary rather than
//! guarded against at each of the four places downstream that would otherwise have
//! to remember.
//!
//! Real Miro data contains `points: [{"x":0,"y":0}]` — a single-point `paint`
//! widget, drawn by tapping the pen without moving it. It is a legal stroke and it
//! must render as a dot, so "fewer than two points" is a shape to support, not an
//! error to reject.

/// Fallback stroke width, in world px, for a `paint` widget whose style carries no
/// `t` key.
///
/// Miro's own default pen is thin; 2px matches what the reference board's SVG export
/// renders for strokes whose clipboard style omits `t`. It is a fallback, not a
/// measurement — a stroke that reaches this constant has lost information.
pub const DEFAULT_WIDTH: f64 = 2.0;

/// Consecutive points closer together than this are merged.
///
/// World space is f64 and a board is tens of thousands of px across, so 1e-6 px is
/// roughly ten orders of magnitude below anything visible: the threshold can only
/// ever catch points that were meant to be the same point. Anything larger would
/// start deleting real detail from a stroke drawn at deep zoom.
pub const MERGE_EPSILON: f64 = 1e-6;

/// Pressure floor. Zero pressure means zero width, which tessellates to degenerate
/// zero-area triangles and renders as a gap in the middle of a stroke; clamping
/// keeps a light touch thin rather than absent.
pub const MIN_PRESSURE: f64 = 1.0 / 64.0;

/// Pressure ceiling. Guards against a bad digitiser driver reporting a huge value
/// and inflating one stroke into a screen-filling blob.
pub const MAX_PRESSURE: f64 = 16.0;

/// One sampled point along a stroke.
///
/// `pressure` is a multiplier on the stroke's width, not an absolute width, so a
/// stroke can be re-weighted by changing [`Stroke::width`] alone. Miro's `paint`
/// widgets carry no pressure at all — every imported point gets `1.0` — but Vellum's
/// own pen input does, and interpolation has to carry it through smoothing or the
/// resampled stroke would lose its taper.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StrokePoint {
    pub x: f64,
    pub y: f64,
    /// Width multiplier at this point, clamped to [`MIN_PRESSURE`]..=[`MAX_PRESSURE`]
    /// once the point is inside a [`Stroke`].
    pub pressure: f64,
}

impl StrokePoint {
    /// A point with full pressure — the form every imported Miro point takes.
    pub const fn new(x: f64, y: f64) -> Self {
        Self { x, y, pressure: 1.0 }
    }

    pub const fn with_pressure(x: f64, y: f64, pressure: f64) -> Self {
        Self { x, y, pressure }
    }

    pub fn distance_to(self, other: Self) -> f64 {
        (self.x - other.x).hypot(self.y - other.y)
    }

    /// Linear interpolation of position *and* pressure.
    ///
    /// Pressure is interpolated linearly even where position follows a cubic,
    /// because a cubic through four pressure samples can undershoot below zero and
    /// invert the stroke's outline. A linear ramp cannot leave the interval its two
    /// endpoints define.
    pub fn lerp(self, other: Self, t: f64) -> Self {
        Self {
            x: self.x + (other.x - self.x) * t,
            y: self.y + (other.y - self.y) * t,
            pressure: self.pressure + (other.pressure - self.pressure) * t,
        }
    }

    pub(crate) fn is_finite(self) -> bool {
        self.x.is_finite() && self.y.is_finite()
    }
}

/// One freehand stroke: Miro's `paint` widget, in Vellum's world space.
///
/// Points are stored in the widget's own coordinate frame, exactly as Miro's
/// `points` array gives them — relative to the widget's placement, not the canvas
/// origin. Keeping them relative means moving a stroke is a transform update rather
/// than a rewrite of several hundred coordinates, and it keeps the numbers small
/// enough that the f32 cast at tessellation time is lossless in practice.
///
/// The fields are private because the invariants (all coordinates finite, no two
/// consecutive points coincident, width finite and positive) are what the rest of
/// the crate is built on. Anything that could break them has to go back through
/// [`Stroke::new`].
#[derive(Debug, Clone, PartialEq)]
pub struct Stroke {
    points: Vec<StrokePoint>,
    width: f64,
}

impl Default for Stroke {
    fn default() -> Self {
        Self { points: Vec::new(), width: DEFAULT_WIDTH }
    }
}

impl Stroke {
    /// Builds a stroke, dropping non-finite points, merging coincident neighbours
    /// and clamping pressure.
    ///
    /// Dropping rather than rejecting is deliberate. A clipboard payload is
    /// undocumented and reverse-engineered; one corrupt coordinate in a 400-point
    /// stroke should cost that coordinate, not the drawing. What the caller must not
    /// get is a `Stroke` that looks fine and tessellates to garbage.
    pub fn new(points: impl IntoIterator<Item = StrokePoint>, width: f64) -> Self {
        let mut clean: Vec<StrokePoint> = Vec::new();
        for mut p in points {
            if !p.is_finite() {
                continue;
            }
            p.pressure = if p.pressure.is_finite() {
                p.pressure.clamp(MIN_PRESSURE, MAX_PRESSURE)
            } else {
                1.0
            };
            if clean.last().is_some_and(|prev| prev.distance_to(p) <= MERGE_EPSILON) {
                continue;
            }
            clean.push(p);
        }

        let width = if width.is_finite() && width > 0.0 { width } else { DEFAULT_WIDTH };
        Self { points: clean, width }
    }

    /// Builds a stroke from Miro's own representation: the `points` array and the
    /// `t` style key, as carried by `vellum_import`'s `Ink`.
    ///
    /// This is the crate's import seam. It takes the tuple/`Option` shapes that
    /// struct already uses rather than depending on it, so the geometry layer stays
    /// free of the importer and can be tested without a clipboard payload.
    pub fn from_miro(points: &[(f64, f64)], thickness: Option<f64>) -> Self {
        Self::new(
            points.iter().map(|&(x, y)| StrokePoint::new(x, y)),
            thickness.unwrap_or(DEFAULT_WIDTH),
        )
    }

    /// Replaces the points, re-running sanitisation. Used by every stage of the
    /// pipeline so that a smoothed or simplified stroke carries the same guarantees
    /// as the raw one.
    pub(crate) fn with_points(&self, points: impl IntoIterator<Item = StrokePoint>) -> Self {
        Self::new(points, self.width)
    }

    pub fn points(&self) -> &[StrokePoint] {
        &self.points
    }

    pub fn into_points(self) -> Vec<StrokePoint> {
        self.points
    }

    /// Base width in world px — Miro's `t`. Per-point pressure multiplies this.
    pub fn width(&self) -> f64 {
        self.width
    }

    /// Sets the base width, applying the same validity rule as construction.
    pub fn set_width(&mut self, width: f64) {
        self.width = if width.is_finite() && width > 0.0 { width } else { DEFAULT_WIDTH };
    }

    pub fn len(&self) -> usize {
        self.points.len()
    }

    pub fn is_empty(&self) -> bool {
        self.points.is_empty()
    }

    /// Whether this stroke is a single tap. Real Miro boards contain these, and
    /// every stage has to handle one: it has no direction, no segments and no
    /// curvature, but it is still visible ink.
    pub fn is_dot(&self) -> bool {
        self.points.len() == 1
    }

    /// Largest half-width anywhere on the stroke. This is the amount the centreline's
    /// bounding box has to grow by, and the reach a hit test has to allow for.
    pub fn max_half_width(&self) -> f64 {
        let max_pressure =
            self.points.iter().map(|p| p.pressure).fold(0.0_f64, f64::max).max(0.0);
        self.width * max_pressure / 2.0
    }

    /// Whether pressure varies along the stroke.
    ///
    /// Worth knowing because it selects the tessellation path: Miro carries no
    /// pressure, so every imported stroke answers `false` and can take lyon's
    /// well-trodden fixed-width tessellator instead of the variable-width one.
    pub fn pressure_varies(&self) -> bool {
        let mut iter = self.points.iter();
        let Some(first) = iter.next() else { return false };
        iter.any(|p| (p.pressure - first.pressure).abs() > 1e-9)
    }

    /// Total centreline length in world px.
    pub fn length(&self) -> f64 {
        self.points.windows(2).map(|w| w[0].distance_to(w[1])).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn miro_single_point_stroke_survives_as_a_dot() {
        // Verbatim from the reference board: `points: [{"x":0,"y":0}]`.
        let s = Stroke::from_miro(&[(0.0, 0.0)], Some(4.0));
        assert!(s.is_dot());
        assert_eq!(s.width(), 4.0);
    }

    #[test]
    fn empty_input_is_an_empty_stroke_not_an_error() {
        let s = Stroke::from_miro(&[], None);
        assert!(s.is_empty());
        assert_eq!(s.width(), DEFAULT_WIDTH);
        assert_eq!(s.length(), 0.0);
        assert_eq!(s.max_half_width(), 0.0);
    }

    #[test]
    fn duplicate_consecutive_points_are_merged() {
        let s = Stroke::from_miro(&[(0.0, 0.0), (0.0, 0.0), (0.0, 0.0), (10.0, 0.0)], None);
        assert_eq!(s.len(), 2);
        assert_eq!(s.points()[1].x, 10.0);
    }

    /// Only *consecutive* duplicates merge. A stroke that returns to an earlier
    /// point is a legitimate self-intersecting drawing, not noise.
    #[test]
    fn a_revisited_point_is_kept() {
        let s = Stroke::from_miro(&[(0.0, 0.0), (10.0, 0.0), (0.0, 0.0)], None);
        assert_eq!(s.len(), 3);
    }

    #[test]
    fn non_finite_coordinates_are_dropped_and_the_rest_survives() {
        let s = Stroke::new(
            [
                StrokePoint::new(0.0, 0.0),
                StrokePoint::new(f64::NAN, 5.0),
                StrokePoint::new(10.0, 0.0),
                StrokePoint::new(20.0, f64::INFINITY),
                StrokePoint::new(20.0, 0.0),
            ],
            2.0,
        );
        assert_eq!(s.len(), 3);
        assert!(s.points().iter().all(|p| p.is_finite()));
    }

    #[test]
    fn width_falls_back_when_absent_or_nonsensical() {
        assert_eq!(Stroke::from_miro(&[], Some(0.0)).width(), DEFAULT_WIDTH);
        assert_eq!(Stroke::from_miro(&[], Some(-3.0)).width(), DEFAULT_WIDTH);
        assert_eq!(Stroke::from_miro(&[], Some(f64::NAN)).width(), DEFAULT_WIDTH);
        assert_eq!(Stroke::from_miro(&[], Some(8.5)).width(), 8.5);
    }

    #[test]
    fn pressure_is_clamped_into_a_renderable_range() {
        let s = Stroke::new(
            [
                StrokePoint::with_pressure(0.0, 0.0, 0.0),
                StrokePoint::with_pressure(10.0, 0.0, 1000.0),
                StrokePoint::with_pressure(20.0, 0.0, f64::NAN),
            ],
            2.0,
        );
        assert_eq!(s.points()[0].pressure, MIN_PRESSURE);
        assert_eq!(s.points()[1].pressure, MAX_PRESSURE);
        assert_eq!(s.points()[2].pressure, 1.0);
    }

    #[test]
    fn pressure_variation_is_detected() {
        assert!(!Stroke::from_miro(&[(0.0, 0.0), (1.0, 1.0)], None).pressure_varies());
        assert!(
            Stroke::new(
                [StrokePoint::new(0.0, 0.0), StrokePoint::with_pressure(1.0, 1.0, 0.5)],
                2.0
            )
            .pressure_varies()
        );
    }

    #[test]
    fn max_half_width_accounts_for_pressure() {
        let s = Stroke::new(
            [StrokePoint::new(0.0, 0.0), StrokePoint::with_pressure(10.0, 0.0, 3.0)],
            4.0,
        );
        assert_eq!(s.max_half_width(), 6.0);
    }

    #[test]
    fn length_sums_the_centreline() {
        let s = Stroke::from_miro(&[(0.0, 0.0), (3.0, 4.0), (3.0, 14.0)], None);
        assert_eq!(s.length(), 15.0);
    }

    #[test]
    fn lerp_carries_pressure() {
        let a = StrokePoint::with_pressure(0.0, 0.0, 0.25);
        let b = StrokePoint::with_pressure(10.0, 20.0, 0.75);
        let m = a.lerp(b, 0.5);
        assert_eq!((m.x, m.y, m.pressure), (5.0, 10.0, 0.5));
    }
}
