//! Charts on the board: the document token, text metrics, and mark tessellation.
//!
//! `vellum-chart` turns data into geometry — nice-number axis ticks, band and value
//! scales, bar/line/area/scatter/pie marks, legends and label collision policy — and
//! draws none of it. Like `vellum-table` it has been complete and tested since before
//! the document could store a chart.
//!
//! Three things live here.
//!
//! # The token
//!
//! An opaque string, as [`crate::shapes`] and [`crate::table`] use, for the layering
//! reason those record.
//!
//! # The metrics
//!
//! `vellum-chart` measures label widths through a [`TextMetrics`] it is given. Its own
//! default is a fixed advance per character — exact for the monospaced numerals an axis
//! is labelled with, and *deliberately generous* for proportional text. Generous is the
//! safe direction for axis gutters, so unlike the table's monospace stand-in this one
//! is not wrong, only loose. [`ShapedMetrics`] tightens it with the real engine, which
//! matters most for category labels, which are words rather than numbers.
//!
//! # The tessellation
//!
//! Marks arrive as rectangles, polylines, closed rings and arcs. Rectangles are quads;
//! everything else becomes a [`vellum_shapes::Mesh`] here, because the renderer's mesh
//! batch is the only path that takes arbitrary triangles.

use vellum_chart::{Arc, ChartGeometry, ChartSpec, Point as ChartPoint, Polyline, Rect as ChartRect};
use vellum_shapes::Mesh;
use vellum_text::{LayoutParams, StyledText, TextEngine};

/// The token stored in the document for a chart.
pub fn encode(spec: &ChartSpec) -> String {
    serde_json::to_string(spec).unwrap_or_else(|error| {
        log::warn!("a chart would not encode ({error}); storing nothing");
        String::new()
    })
}

/// The chart a token names, or a small default when it cannot be read.
pub fn decode(token: &str) -> ChartSpec {
    serde_json::from_str(token).unwrap_or_else(|error| {
        if !token.is_empty() {
            log::warn!("unreadable chart ({error}); drawing an empty bar chart");
        }
        default_chart()
    })
}

/// What the chart tool places: a small grouped bar chart with real numbers in it.
///
/// Not an empty chart. An empty one draws axes around nothing, which looks like a
/// failure rather than like something waiting for data — and there is no data editor
/// yet to fill it in.
pub fn default_chart() -> ChartSpec {
    use vellum_chart::{ChartKind, Dataset, Series};
    let data = Dataset::new(
        ["Q1", "Q2", "Q3", "Q4"].map(str::to_owned).to_vec(),
        vec![
            Series::new("Actual", vec![12.0, 19.0, 15.0, 24.0]),
            Series::new("Target", vec![15.0, 15.0, 20.0, 20.0]),
        ],
    );
    ChartSpec::categorical(ChartKind::bar(), data)
}

/// Every label on the chart — its categories and its series names — for search and export.
///
/// The numbers are deliberately left out. A search for "24" matching every chart that
/// happens to plot a 24 would drown the results a person was looking for, and a value is
/// not a word anybody names the mark by. See [`crate::words`].
pub fn words(spec: &ChartSpec) -> Vec<String> {
    use vellum_chart::ChartData;
    let mut out = Vec::new();
    match &spec.data {
        ChartData::Categorical(data) => {
            out.extend(data.categories().iter().cloned());
            out.extend(data.series().iter().map(|series| series.name.clone()));
        }
        // A scatter's points have no categories; only the series are named.
        ChartData::Points(points) => {
            out.extend(points.series().iter().map(|series| series.name.clone()));
        }
    }
    out.retain(|text| !text.trim().is_empty());
    out
}

/// Label metrics backed by the real font stack.
pub struct ShapedMetrics<'a> {
    engine: std::cell::RefCell<&'a mut TextEngine>,
}

impl<'a> ShapedMetrics<'a> {
    pub fn new(engine: &'a mut TextEngine) -> Self {
        Self { engine: std::cell::RefCell::new(engine) }
    }
}

impl vellum_chart::TextMetrics for ShapedMetrics<'_> {
    /// `TextMetrics::width` takes `&self` — measuring is conceptually a question, not a
    /// mutation — but shaping needs `&mut` on the engine, so the borrow is moved to
    /// runtime. Single-threaded and never re-entrant: `vellum-chart` calls this from
    /// its own layout and holds no borrow of its own across the call.
    fn width(&self, text: &str, size: f32) -> f32 {
        let params = LayoutParams { font_size: size, max_width: None, ..LayoutParams::default() };
        self.engine.borrow_mut().measure(&StyledText::plain(text), &params).width
    }
}

/// The chart's geometry, laid out to fill `size`.
pub fn build(spec: &ChartSpec, engine: &mut TextEngine, size: (f64, f64)) -> ChartGeometry {
    #[expect(clippy::cast_possible_truncation, reason = "an item's box is screen-scale")]
    let frame = ChartRect::new(0.0, 0.0, size.0 as f32, size.1 as f32);
    let metrics = ShapedMetrics::new(engine);
    vellum_chart::build(spec, frame, &metrics)
}

/// How finely an arc is flattened: one segment per this many degrees.
///
/// Six is invisible at any zoom a pie is read at, and it keeps a full circle to sixty
/// triangles rather than the hundreds a per-pixel tolerance would ask for on a chart
/// that is a few hundred units across.
const ARC_STEP_DEGREES: f32 = 6.0;

/// A pie or donut slice as triangles.
///
/// A donut is a quad strip between the two radii; a pie is a fan from the centre. Both
/// are the same loop with the inner radius pinned to zero, so there is one path rather
/// than two that must agree.
pub fn slice_mesh(arc: &Arc) -> Mesh {
    let sweep = arc.sweep().abs();
    if sweep <= f32::EPSILON || arc.outer_radius <= 0.0 {
        return Mesh::default();
    }
    let steps = ((sweep.to_degrees() / ARC_STEP_DEGREES).ceil() as usize).max(1);
    let inner = arc.inner_radius.max(0.0).min(arc.outer_radius);

    let mut vertices = Vec::with_capacity((steps + 1) * 2);
    let mut indices = Vec::with_capacity(steps * 6);
    for step in 0..=steps {
        let t = step as f32 / steps as f32;
        let angle = arc.start_angle + (arc.end_angle - arc.start_angle) * t;
        let (sin, cos) = angle.sin_cos();
        vertices.push([arc.centre.x + cos * inner, arc.centre.y + sin * inner]);
        vertices.push([arc.centre.x + cos * arc.outer_radius, arc.centre.y + sin * arc.outer_radius]);
    }
    for step in 0..steps {
        let base = (step * 2) as u32;
        // Two triangles per step. For a pie the inner pair are all the centre, so the
        // first degenerates to zero area and costs a triangle rather than a branch.
        indices.extend_from_slice(&[base, base + 1, base + 3]);
        indices.extend_from_slice(&[base, base + 3, base + 2]);
    }
    Mesh { vertices, indices }
}

/// A closed ring as triangles, by fanning from its centroid.
///
/// Good enough for an **area** mark and only for that: `vellum-chart` builds those as a
/// run out along the values and back along the baseline, which is monotone in x and
/// therefore star-shaped about its own centroid. A general polygon needs a real
/// tessellator, and this must not be pointed at one.
pub fn ring_mesh(ring: &Polyline) -> Mesh {
    let points = &ring.points;
    if points.len() < 3 {
        return Mesh::default();
    }
    let n = points.len() as f32;
    let centroid = points.iter().fold(ChartPoint::new(0.0, 0.0), |acc, p| {
        ChartPoint::new(acc.x + p.x / n, acc.y + p.y / n)
    });

    let mut vertices = Vec::with_capacity(points.len() + 1);
    vertices.push([centroid.x, centroid.y]);
    vertices.extend(points.iter().map(|p| [p.x, p.y]));

    let mut indices = Vec::with_capacity(points.len() * 3);
    for i in 0..points.len() {
        let a = (i + 1) as u32;
        let b = ((i + 1) % points.len() + 1) as u32;
        indices.extend_from_slice(&[0, a, b]);
    }
    Mesh { vertices, indices }
}

/// A line series as a triangle strip of the given width.
///
/// The stroking itself is [`crate::mesh::ribbon`] — a mind map's branches need exactly
/// the same ribbon, and `vellum-chart` and `vellum-mindmap` each define their own point
/// type, so the shared code is written over `[f32; 2]` and each caller converts.
pub fn polyline_mesh(line: &Polyline, width: f32) -> Mesh {
    let points: Vec<[f32; 2]> = line.points.iter().map(|p| [p.x, p.y]).collect();
    crate::mesh::ribbon(&points, width)
}

#[cfg(test)]
mod tests {
    use super::*;
    use vellum_chart::Mark;

    fn engine() -> TextEngine {
        TextEngine::new().expect("the test machine has fonts")
    }

    #[test]
    fn a_chart_survives_the_trip_to_the_document_and_back() {
        let spec = default_chart();
        assert_eq!(decode(&encode(&spec)), spec);
    }

    #[test]
    fn an_unreadable_token_becomes_a_default_chart_rather_than_failing() {
        assert_eq!(decode("{ not json"), default_chart());
        assert_eq!(decode(""), default_chart());
    }

    /// The default has data in it. An empty chart draws axes around nothing, which
    /// reads as a failure rather than as something waiting to be filled in — and there
    /// is no data editor yet to fill it.
    #[test]
    fn the_default_chart_has_marks() {
        let mut engine = engine();
        let geometry = build(&default_chart(), &mut engine, (480.0, 320.0));
        assert!(!geometry.marks.is_empty(), "a placed chart would have been blank");
        assert!(geometry.marks.iter().all(|m| matches!(m, Mark::Bar(_))));
        assert!(!geometry.axes.is_empty(), "a bar chart has axes");
    }

    /// Shaping beats the fixed-advance default where the labels are *words*. The
    /// crate's own metric is exact for numerals and generous for prose, so a category
    /// label is where the difference shows.
    #[test]
    fn shaped_metrics_are_tighter_than_the_fixed_advance_for_words() {
        use vellum_chart::TextMetrics;
        let mut engine = engine();
        let shaped = ShapedMetrics::new(&mut engine);
        let fixed = vellum_chart::text::MonoMetrics::default();

        let word = "Category";
        assert!(
            shaped.width(word, 14.0) < fixed.width(word, 14.0),
            "shaped {} was not tighter than fixed {}",
            shaped.width(word, 14.0),
            fixed.width(word, 14.0),
        );
    }

    /// A pie slice is a fan; a donut slice is a strip. Both come out of one loop, so
    /// the two cannot disagree about where the arc is.
    #[test]
    fn a_slice_tessellates_to_triangles_inside_its_own_radius() {
        let arc = Arc {
            centre: ChartPoint::new(100.0, 100.0),
            inner_radius: 0.0,
            outer_radius: 50.0,
            start_angle: 0.0,
            end_angle: std::f32::consts::FRAC_PI_2,
        };
        let mesh = slice_mesh(&arc);
        assert!(mesh.triangle_count() > 0);
        assert_eq!(mesh.indices.len() % 3, 0);
        for [x, y] in &mesh.vertices {
            let r = (x - 100.0).hypot(y - 100.0);
            assert!(r <= 50.0 + 1e-3, "vertex at radius {r} escaped the slice");
        }

        // A donut keeps its hole: no vertex lands inside the inner radius.
        let donut = Arc { inner_radius: 30.0, ..arc };
        for [x, y] in &slice_mesh(&donut).vertices {
            let r = (x - 100.0).hypot(y - 100.0);
            assert!(r >= 30.0 - 1e-3, "vertex at radius {r} filled the hole");
        }
    }

    /// A zero sweep is a slice of nothing, not a degenerate triangle.
    #[test]
    fn an_empty_slice_tessellates_to_nothing() {
        let arc = Arc {
            centre: ChartPoint::new(0.0, 0.0),
            inner_radius: 0.0,
            outer_radius: 50.0,
            start_angle: 1.0,
            end_angle: 1.0,
        };
        assert_eq!(slice_mesh(&arc).triangle_count(), 0);
    }

    /// A line's ribbon is the stroke width across, whichever way the segment runs.
    #[test]
    fn a_polyline_becomes_a_ribbon_of_the_right_width() {
        let line = Polyline::new(vec![ChartPoint::new(0.0, 0.0), ChartPoint::new(100.0, 0.0)]);
        let mesh = polyline_mesh(&line, 4.0);
        assert_eq!(mesh.triangle_count(), 2, "one segment is two triangles");

        let ys: Vec<f32> = mesh.vertices.iter().map(|[_, y]| *y).collect();
        let (min, max) = (
            ys.iter().copied().fold(f32::MAX, f32::min),
            ys.iter().copied().fold(f32::MIN, f32::max),
        );
        assert!((max - min - 4.0).abs() < 1e-3, "ribbon was {} wide", max - min);
    }

    /// Degenerate input produces no triangles rather than NaNs. A repeated point in a
    /// series is ordinary data, not a bug to crash on.
    #[test]
    fn a_degenerate_polyline_produces_nothing() {
        let same = Polyline::new(vec![ChartPoint::new(5.0, 5.0), ChartPoint::new(5.0, 5.0)]);
        assert_eq!(polyline_mesh(&same, 4.0).triangle_count(), 0);
        assert_eq!(polyline_mesh(&Polyline::new(vec![]), 4.0).triangle_count(), 0);

        let single = Polyline::new(vec![ChartPoint::new(0.0, 0.0)]);
        assert_eq!(ring_mesh(&single).triangle_count(), 0);
    }

    /// An area ring fans from its centroid and covers it — the property that makes the
    /// fan valid for the shape `vellum-chart` actually produces.
    #[test]
    fn an_area_ring_fans_into_triangles() {
        let ring = Polyline::new(vec![
            ChartPoint::new(0.0, 100.0),
            ChartPoint::new(50.0, 40.0),
            ChartPoint::new(100.0, 70.0),
            ChartPoint::new(100.0, 100.0),
        ]);
        let mesh = ring_mesh(&ring);
        assert_eq!(mesh.triangle_count(), 4, "one triangle per edge");
        assert_eq!(mesh.vertices.len(), 5, "the centroid plus every point");
    }
}

