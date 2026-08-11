//! Lines, areas and scatters — the marks built from points rather than rectangles.
//!
//! # A gap breaks the line
//!
//! A series with a hole in it becomes *several* polylines, not one. Joining across a
//! missing month draws a straight segment through values nobody measured, and it is
//! indistinguishable from data. Breaking the line says "not measured" without a
//! legend entry for it.
//!
//! A run of exactly one point has no line to draw, so it is emitted as a dot even
//! when markers are off. Otherwise a single measurement surrounded by gaps would be
//! invisible — the one case where a chart silently loses a value it was given.
//!
//! # Stacked areas stack on what is there
//!
//! A stacked band rides on the running total of the series *below* it, counting only
//! values that exist. Where a lower series has a hole, the ones above it settle down
//! into the space rather than floating over it, which is what "there is nothing
//! under this" should look like.

use crate::chart::{LabelTarget, PlotContext, labelled};
use crate::data::{Dataset, PointSet};
use crate::geom::{Point, Polyline, Rect};
use crate::label::Placement;
use crate::mark::{Area, Dot, Line, Mark};
use crate::scale::{BandScale, ValueScale};

/// A maximal run of consecutive categories where a series has values.
struct Run {
    /// Category index of the first point, for labelling.
    start: usize,
    points: Vec<Point>,
    /// The value at each point, parallel to `points`.
    values: Vec<f64>,
    /// For a stacked area: the lower edge of the band, already in pixels.
    baseline: Vec<f32>,
}

/// Splits a series into runs, mapping each present value to a point.
fn runs(
    series_index: usize,
    data: &Dataset,
    scale: &ValueScale,
    bands: &BandScale,
    plot: Rect,
    stack_below: Option<&[f64]>,
) -> Vec<Run> {
    let series = &data.series()[series_index];
    let mut runs: Vec<Run> = Vec::new();
    let mut current: Option<Run> = None;

    for index in 0..data.category_count() {
        match series.value(index) {
            Some(value) => {
                let below = stack_below.map_or(0.0, |below| below[index]);
                let point = Point::new(
                    bands.centre(index),
                    scale.position(below + value, plot.bottom(), plot.top()),
                );
                let baseline = scale.position(below, plot.bottom(), plot.top());
                let run = current.get_or_insert_with(|| Run {
                    start: index,
                    points: Vec::new(),
                    values: Vec::new(),
                    baseline: Vec::new(),
                });
                run.points.push(point);
                run.values.push(value);
                run.baseline.push(baseline);
            }
            None => {
                if let Some(run) = current.take() {
                    runs.push(run);
                }
            }
        }
    }
    runs.extend(current);
    runs
}

/// Line series, with optional markers at each point.
pub(crate) fn build_lines(
    data: &Dataset,
    scale: &ValueScale,
    bands: &BandScale,
    plot: Rect,
    markers: bool,
    context: &mut PlotContext,
) -> Vec<Mark> {
    if data.is_empty() || plot.is_empty() {
        return Vec::new();
    }
    let mut marks = Vec::new();
    let mut dots = Vec::new();

    for series_index in 0..data.series_count() {
        let colour = context.palette.series(data.colour_slot(series_index));
        for run in runs(series_index, data, scale, bands, plot, None) {
            if run.points.len() > 1 {
                marks.push(Mark::Line(Line {
                    series: series_index,
                    path: Polyline::new(run.points.clone()),
                    width: context.style.line_width,
                    colour,
                }));
            }
            // A lone point has no line; it is drawn regardless of the marker
            // setting, or the value would vanish.
            let show = markers || run.points.len() == 1;
            if show {
                for point in &run.points {
                    dots.push(Mark::Dot(Dot {
                        series: series_index,
                        index: run.start,
                        centre: *point,
                        radius: context.style.dot_radius,
                        colour,
                        ring_width: context.style.dot_ring,
                        ring_colour: context.palette.surface,
                    }));
                }
            }
            label_run(context, series_index, data, &run, colour);
        }
    }
    // Dots last: they sit on top of every line, which is what the surface ring is
    // for — a dot crossing its own line stays countable.
    marks.extend(dots);
    marks
}

/// Area series: a wash under the line, plus the line itself.
pub(crate) fn build_areas(
    data: &Dataset,
    scale: &ValueScale,
    bands: &BandScale,
    plot: Rect,
    stacked: bool,
    context: &mut PlotContext,
) -> Vec<Mark> {
    if data.is_empty() || plot.is_empty() {
        return Vec::new();
    }
    let mut marks = Vec::new();
    let mut lines = Vec::new();
    let mut totals = vec![0.0_f64; data.category_count()];

    for series_index in 0..data.series_count() {
        let colour = context.palette.series(data.colour_slot(series_index));
        let below = stacked.then(|| totals.clone());
        for run in runs(series_index, data, scale, bands, plot, below.as_deref()) {
            if run.points.len() > 1 {
                // Out along the values, back along whatever the band stands on.
                let mut outline = run.points.clone();
                outline.extend(
                    run.points
                        .iter()
                        .zip(&run.baseline)
                        .rev()
                        .map(|(point, baseline)| Point::new(point.x, *baseline)),
                );
                marks.push(Mark::Area(Area {
                    series: series_index,
                    outline: Polyline::new(outline),
                    fill: colour.with_alpha(crate::palette::Palette::AREA_FILL_ALPHA),
                }));
                lines.push(Mark::Line(Line {
                    series: series_index,
                    path: Polyline::new(run.points.clone()),
                    width: context.style.line_width,
                    colour,
                }));
            } else if run.points.len() == 1 {
                lines.push(Mark::Dot(Dot {
                    series: series_index,
                    index: run.start,
                    centre: run.points[0],
                    radius: context.style.dot_radius,
                    colour,
                    ring_width: context.style.dot_ring,
                    ring_colour: context.palette.surface,
                }));
            }
            label_run(context, series_index, data, &run, colour);
        }
        if stacked {
            for (index, total) in totals.iter_mut().enumerate() {
                if let Some(value) = data.series()[series_index].value(index) {
                    *total += value;
                }
            }
        }
    }
    // Every wash first, then every line: a line must never be dimmed by the next
    // series' fill drawn over it.
    marks.extend(lines);
    marks
}

/// Free points on two numeric axes.
pub(crate) fn build_scatter(
    points: &PointSet,
    x_scale: &ValueScale,
    y_scale: &ValueScale,
    plot: Rect,
    context: &mut PlotContext,
) -> Vec<Mark> {
    if points.is_empty() || plot.is_empty() {
        return Vec::new();
    }
    let mut marks = Vec::new();
    for (series_index, series) in points.series().iter().enumerate() {
        let colour = context.palette.series(points.colour_slot(series_index));
        // The extremes here are by y, which is what a reader looks for in a cloud:
        // the highest and lowest response, not the first and last row.
        let extremes = extremes_by_y(series.points());
        for (index, &[x, y]) in series.points().iter().enumerate() {
            let centre = Point::new(
                x_scale.position(x, plot.left(), plot.right()),
                y_scale.position(y, plot.bottom(), plot.top()),
            );
            marks.push(Mark::Dot(Dot {
                series: series_index,
                index,
                centre,
                radius: context.style.dot_radius,
                colour,
                ring_width: context.style.dot_ring,
                ring_colour: context.palette.surface,
            }));
            if labelled(context.style.labels, index, series.len(), extremes) {
                label_point(context, series_index, index, y, centre, colour);
            }
        }
    }
    marks
}

fn extremes_by_y(points: &[[f64; 2]]) -> Option<(usize, usize)> {
    let mut low: Option<(usize, f64)> = None;
    let mut high: Option<(usize, f64)> = None;
    for (index, [_, y]) in points.iter().enumerate() {
        if low.is_none_or(|(_, value)| *y < value) {
            low = Some((index, *y));
        }
        if high.is_none_or(|(_, value)| *y > value) {
            high = Some((index, *y));
        }
    }
    Some((low?.0, high?.0))
}

/// Labels the points of one run according to the policy.
fn label_run(
    context: &mut PlotContext,
    series_index: usize,
    data: &Dataset,
    run: &Run,
    colour: crate::colour::Colour,
) {
    if context.style.labels == crate::label::LabelPolicy::None {
        return;
    }
    let series = &data.series()[series_index];
    let extremes = series.extreme_indices();
    for (offset, point) in run.points.iter().enumerate() {
        let index = run.start + offset;
        if !labelled(context.style.labels, index, series.len(), extremes) {
            continue;
        }
        label_point(context, series_index, index, run.values[offset], *point, colour);
    }
}

/// Offers a point its label: above first, then below, then to either side.
///
/// Above is preferred because a line's local maximum is the point a reader looks at,
/// and a number under a rising line collides with the line itself. The alternatives
/// exist so that a cluster of points does not lose every label to the first
/// collision — and when they all fail, the label is dropped rather than stacked on
/// top of its neighbour.
fn label_point(
    context: &mut PlotContext,
    series: usize,
    index: usize,
    value: f64,
    centre: Point,
    colour: crate::colour::Colour,
) {
    let text = context.format(value);
    let gap = context.style.text_gap;
    let radius = context.style.dot_radius + context.style.dot_ring;
    let ink = context.palette.ink_on(colour);

    let target = LabelTarget { series, index, value, anchor: centre, ink };
    context.try_label(target, text, move |width, line| {
        vec![
            (
                Rect::new(centre.x - width * 0.5, centre.y - radius - gap - line, width, line),
                Placement::Above,
            ),
            (
                Rect::new(centre.x - width * 0.5, centre.y + radius + gap, width, line),
                Placement::Below,
            ),
            (
                Rect::new(centre.x + radius + gap, centre.y - line * 0.5, width, line),
                Placement::Right,
            ),
            (
                Rect::new(centre.x - radius - gap - width, centre.y - line * 0.5, width, line),
                Placement::Left,
            ),
        ]
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chart::{ChartData, ChartGeometry, ChartKind, ChartSpec, build};
    use crate::data::{PointSeries, Series};
    use crate::label::LabelPolicy;
    use crate::style::{ChartStyle, LegendPosition};
    use crate::text::MonoMetrics;

    fn frame() -> Rect {
        Rect::new(0.0, 0.0, 480.0, 320.0)
    }

    fn chart(kind: ChartKind, data: Dataset, style: ChartStyle) -> ChartGeometry {
        build(
            &ChartSpec::new(kind, ChartData::Categorical(data)).with_style(style),
            frame(),
            &MonoMetrics::default(),
        )
    }

    fn paths(geometry: &ChartGeometry) -> Vec<&Line> {
        geometry
            .marks
            .iter()
            .filter_map(|mark| match mark {
                Mark::Line(line) => Some(line),
                _ => None,
            })
            .collect()
    }

    fn dots(geometry: &ChartGeometry) -> Vec<&Dot> {
        geometry
            .marks
            .iter()
            .filter_map(|mark| match mark {
                Mark::Dot(dot) => Some(dot),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_line_visits_every_category_centre_in_order() {
        let data = Dataset::new(["a", "b", "c"], [Series::new("s", [3.0, 9.0, 6.0])]);
        let geometry = chart(ChartKind::line(), data, ChartStyle::default());
        let path = paths(&geometry)[0];
        assert_eq!(path.path.len(), 3);
        for pair in path.path.points.windows(2) {
            assert!(pair[1].x > pair[0].x, "points must run left to right");
        }
        assert!(path.path.points.iter().all(|p| geometry.plot.contains(*p)));
    }

    /// The rule the module exists for: a hole is a hole, not a straight line through
    /// values nobody measured.
    #[test]
    fn a_gap_splits_the_line_rather_than_being_joined_across() {
        let data = Dataset::new(
            ["a", "b", "c", "d"],
            [Series::with_gaps("s", [Some(1.0), None, Some(5.0), Some(6.0)])],
        );
        let geometry = chart(ChartKind::line(), data, ChartStyle::default());
        let paths = paths(&geometry);
        assert_eq!(paths.len(), 1, "only the c-d run is long enough to be a line");
        assert_eq!(paths[0].path.len(), 2);
        // The isolated first point still shows up.
        assert!(dots(&geometry).iter().any(|dot| dot.centre.x < paths[0].path.points[0].x));
    }

    #[test]
    fn a_lone_point_is_drawn_even_with_markers_off() {
        let data = Dataset::new(
            ["a", "b", "c"],
            [Series::with_gaps("s", [Some(4.0), None, None])],
        );
        let geometry = chart(ChartKind::Line { markers: false }, data, ChartStyle::default());
        assert!(paths(&geometry).is_empty());
        assert_eq!(dots(&geometry).len(), 1);
    }

    #[test]
    fn markers_carry_a_surface_ring_so_they_stay_countable() {
        let data = Dataset::new(["a", "b"], [Series::new("s", [1.0, 2.0])]);
        let geometry = chart(ChartKind::line(), data, ChartStyle::default());
        for dot in dots(&geometry) {
            assert_eq!(dot.ring_colour, geometry.surface);
            assert!(dot.ring_width > 0.0);
            assert!(dot.radius * 2.0 >= 8.0, "8px is the minimum readable marker");
        }
    }

    #[test]
    fn dots_are_drawn_after_the_lines_they_sit_on() {
        let data = Dataset::new(["a", "b"], [Series::new("s", [1.0, 2.0])]);
        let geometry = chart(ChartKind::line(), data, ChartStyle::default());
        let first_dot = geometry.marks.iter().position(|m| matches!(m, Mark::Dot(_))).unwrap();
        let last_line = geometry.marks.iter().rposition(|m| matches!(m, Mark::Line(_))).unwrap();
        assert!(first_dot > last_line);
    }

    #[test]
    fn an_area_closes_back_along_its_baseline() {
        let data = Dataset::new(["a", "b", "c"], [Series::new("s", [3.0, 9.0, 6.0])]);
        let geometry = chart(ChartKind::area(), data, ChartStyle::default());
        let area = geometry
            .marks
            .iter()
            .find_map(|mark| match mark {
                Mark::Area(area) => Some(area),
                _ => None,
            })
            .unwrap();
        assert_eq!(area.outline.len(), 6, "three points out, three back");
        let baseline = geometry.baseline.unwrap().from.y;
        for point in &area.outline.points[3..] {
            assert!((point.y - baseline).abs() < 1e-3);
        }
        assert!(area.fill.a < 64, "an area fill is a wash, not a block");
    }

    #[test]
    fn every_wash_is_drawn_before_every_line() {
        let data = Dataset::new(
            ["a", "b"],
            [Series::new("x", [3.0, 9.0]), Series::new("y", [5.0, 2.0])],
        );
        let geometry = chart(ChartKind::Area { stacked: false }, data, ChartStyle::default());
        let last_area = geometry.marks.iter().rposition(|m| matches!(m, Mark::Area(_))).unwrap();
        let first_line = geometry.marks.iter().position(|m| matches!(m, Mark::Line(_))).unwrap();
        assert!(first_line > last_area);
    }

    #[test]
    fn stacked_areas_ride_on_the_series_below_them() {
        let data = Dataset::new(
            ["a", "b"],
            [Series::new("x", [10.0, 10.0]), Series::new("y", [10.0, 10.0])],
        );
        let geometry = chart(ChartKind::Area { stacked: true }, data, ChartStyle::default());
        let areas: Vec<&Area> = geometry
            .marks
            .iter()
            .filter_map(|mark| match mark {
                Mark::Area(area) => Some(area),
                _ => None,
            })
            .collect();
        assert_eq!(areas.len(), 2);
        // The second band sits entirely above the first: y decreases upwards.
        let top_of_first = areas[0].outline.points[0].y;
        let top_of_second = areas[1].outline.points[0].y;
        assert!(top_of_second < top_of_first);
    }

    #[test]
    fn a_scatter_places_a_dot_per_point_and_crops_its_axes() {
        let points = PointSet::new([PointSeries::new(
            "run",
            [[100.0, 51.0], [110.0, 52.5], [120.0, 49.0]],
        )]);
        let geometry = build(
            &ChartSpec::scatter(points),
            frame(),
            &MonoMetrics::default(),
        );
        assert_eq!(dots(&geometry).len(), 3);
        assert_eq!(geometry.axes.len(), 2);
        // Neither axis was forced to zero — a scatter is read as a shape.
        assert!(geometry.axes[0].ticks[0].value > 40.0);
        assert!(geometry.axes[1].ticks[0].value > 50.0);
        assert!(dots(&geometry).iter().all(|dot| geometry.plot.contains(dot.centre)));
    }

    #[test]
    fn scatter_labels_go_to_the_extremes_by_value() {
        let points = PointSet::new([PointSeries::new(
            "run",
            [[1.0, 10.0], [2.0, 90.0], [3.0, 50.0], [4.0, 5.0]],
        )]);
        let style = ChartStyle::default()
            .with_labels(LabelPolicy::Extremes)
            .with_legend(LegendPosition::None);
        let geometry = build(
            &ChartSpec::scatter(points).with_style(style),
            frame(),
            &MonoMetrics::default(),
        );
        let labelled: Vec<f64> = geometry.labels.iter().map(|label| label.value).collect();
        assert_eq!(labelled.len(), 2);
        assert!(labelled.contains(&90.0) && labelled.contains(&5.0));
    }

    #[test]
    fn line_labels_never_overlap() {
        let data = Dataset::new(
            ["a", "b", "c", "d", "e", "f"],
            [Series::new("s", [10.0, 11.0, 10.5, 11.2, 10.8, 11.1])],
        );
        let style = ChartStyle::default()
            .with_labels(LabelPolicy::All)
            .with_legend(LegendPosition::None);
        let geometry = chart(ChartKind::line(), data, style);
        let boxes: Vec<Rect> = geometry.labels.iter().map(|l| l.label.rect).collect();
        for (i, a) in boxes.iter().enumerate() {
            for b in &boxes[i + 1..] {
                assert!(!a.intersects(b), "{a:?} overlaps {b:?}");
            }
        }
    }

    #[test]
    fn an_empty_point_set_still_produces_two_axes() {
        let geometry = build(
            &ChartSpec::scatter(PointSet::default()),
            frame(),
            &MonoMetrics::default(),
        );
        assert!(geometry.marks.is_empty());
        assert!(geometry.notes.empty);
        assert_eq!(geometry.axes.len(), 2);
        assert!(geometry.is_finite());
    }

    #[test]
    fn a_single_point_gets_an_axis_around_it_rather_than_a_flat_one() {
        let points = PointSet::new([PointSeries::new("one", [[7.0, 7.0]])]);
        let geometry = build(
            &ChartSpec::scatter(points),
            frame(),
            &MonoMetrics::default(),
        );
        assert_eq!(dots(&geometry).len(), 1);
        for axis in &geometry.axes {
            assert!(axis.ticks.len() >= 2);
        }
        assert!(geometry.is_finite());
    }
}
