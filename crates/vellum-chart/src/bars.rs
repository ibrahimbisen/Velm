//! Bars: grouped, stacked, vertical and horizontal.
//!
//! One module for all four because they differ in two decisions, not four
//! implementations: which axis the value runs along, and whether series sit beside
//! each other or end to end. Writing them separately is how a rounded corner ends up
//! on three of them and a gap on two.
//!
//! # Two rules that are easy to get wrong
//!
//! **A bar grows from zero, and is square there.** The rounded end is the value end
//! only. Rounding both ends detaches the bar from its baseline, and at small heights
//! it shortens the bar the reader measures.
//!
//! **Bars are capped, not stretched.** A category band wider than
//! [`ChartStyle::bar_max_thickness`](crate::style::ChartStyle::bar_max_thickness)
//! keeps a 24px bar with the slack as air. A bar as wide as it is tall has stopped
//! reading as a measured length and become a block of colour.

use crate::chart::{Grouping, LabelTarget, Orientation, PlotContext, labelled};
use crate::data::Dataset;
use crate::geom::{Point, Rect};
use crate::label::Placement;
use crate::mark::{Bar, BarEnd, Mark, bar_radius};
use crate::scale::{BandScale, ValueScale};

/// Builds every bar in the chart, in series-then-category order.
pub(crate) fn build(
    data: &Dataset,
    scale: &ValueScale,
    bands: &BandScale,
    plot: Rect,
    orientation: Orientation,
    grouping: Grouping,
    context: &mut PlotContext,
) -> Vec<Mark> {
    if data.is_empty() || plot.is_empty() {
        return Vec::new();
    }
    match grouping {
        Grouping::Grouped => grouped(data, scale, bands, plot, orientation, context),
        Grouping::Stacked => stacked(data, scale, bands, plot, orientation, context),
    }
}

/// Where a value sits along the value axis, in pixels.
fn value_position(scale: &ValueScale, value: f64, plot: Rect, orientation: Orientation) -> f32 {
    match orientation {
        // y grows downwards, so the axis minimum is at the bottom.
        Orientation::Vertical => scale.position(value, plot.bottom(), plot.top()),
        Orientation::Horizontal => scale.position(value, plot.left(), plot.right()),
    }
}

fn baseline_position(scale: &ValueScale, plot: Rect, orientation: Orientation) -> f32 {
    match orientation {
        Orientation::Vertical => scale.baseline(plot.bottom(), plot.top()),
        Orientation::Horizontal => scale.baseline(plot.left(), plot.right()),
    }
}

/// A bar from its band and its two value-axis coordinates.
fn bar_rect(orientation: Orientation, band_start: f32, band_size: f32, from: f32, to: f32) -> Rect {
    match orientation {
        Orientation::Vertical => Rect::from_edges(band_start, from, band_start + band_size, to),
        Orientation::Horizontal => Rect::from_edges(from, band_start, to, band_start + band_size),
    }
}

/// Which end carries the round: the one away from the baseline.
fn rounded_end(orientation: Orientation, positive: bool) -> BarEnd {
    match (orientation, positive) {
        (Orientation::Vertical, true) => BarEnd::Top,
        (Orientation::Vertical, false) => BarEnd::Bottom,
        (Orientation::Horizontal, true) => BarEnd::Right,
        (Orientation::Horizontal, false) => BarEnd::Left,
    }
}

/// Caps a bar to its maximum thickness, keeping it centred in the slot it was given.
fn capped(start: f32, size: f32, max: f32) -> (f32, f32) {
    if size <= max { (start, size) } else { (start + (size - max) * 0.5, max) }
}

fn grouped(
    data: &Dataset,
    scale: &ValueScale,
    bands: &BandScale,
    plot: Rect,
    orientation: Orientation,
    context: &mut PlotContext,
) -> Vec<Mark> {
    let series_count = data.series_count();
    let baseline = baseline_position(scale, plot, orientation);
    let mut marks = Vec::new();

    for (series_index, series) in data.series().iter().enumerate() {
        let extremes = series.extreme_indices();
        let colour = context.palette.series(data.colour_slot(series_index));
        for index in 0..data.category_count() {
            let Some(value) = series.value(index) else { continue };
            let (start, size) = bands.sub_band(
                index,
                series_index,
                series_count,
                context.style.surface_gap,
            );
            let (start, size) = capped(start, size, context.style.bar_max_thickness);
            let end = value_position(scale, value, plot, orientation);
            let rect = bar_rect(orientation, start, size, baseline, end);
            marks.push(Mark::Bar(Bar {
                series: series_index,
                index,
                value,
                radius: bar_radius(&rect, context.style.bar_radius),
                rect,
                rounded_end: rounded_end(orientation, value >= 0.0),
                colour,
            }));

            if labelled(context.style.labels, index, series.len(), extremes) {
                label_bar(context, series_index, index, value, &rect, orientation, colour, false);
            }
        }
    }
    marks
}

fn stacked(
    data: &Dataset,
    scale: &ValueScale,
    bands: &BandScale,
    plot: Rect,
    orientation: Orientation,
    context: &mut PlotContext,
) -> Vec<Mark> {
    // No baseline is read here: a stack's first segment starts at the running total
    // of zero, and a stacked chart's axis always contains zero, so
    // `value_position(.., 0.0)` *is* the baseline.
    let gap = context.style.surface_gap;
    let mut marks = Vec::new();

    for index in 0..data.category_count() {
        let (start, size) = capped(
            bands.band_start(index),
            bands.band_width(),
            context.style.bar_max_thickness,
        );
        // Each sign stacks away from zero independently, so a category holding +8
        // and -3 draws both, from the same baseline, in opposite directions.
        let (mut up, mut down) = (0.0_f64, 0.0_f64);
        let mut segments: Vec<(usize, f64, f32, f32, bool)> = Vec::new();

        for (series_index, series) in data.series().iter().enumerate() {
            let Some(value) = series.value(index) else { continue };
            let positive = value >= 0.0;
            let total = if positive { &mut up } else { &mut down };
            let from = value_position(scale, *total, plot, orientation);
            *total += value;
            let to = value_position(scale, *total, plot, orientation);
            segments.push((series_index, value, from, to, positive));
        }

        // The outermost segment of each direction is the one that gets the round;
        // everything nearer the baseline is interior and stays square.
        let outermost_up = segments.iter().rposition(|(_, _, _, _, positive)| *positive);
        let outermost_down = segments.iter().rposition(|(_, _, _, _, positive)| !*positive);

        for (position, (series_index, value, from, to, positive)) in segments.iter().enumerate() {
            let outermost = Some(position) == if *positive { outermost_up } else { outermost_down };
            // The 2px separator is bare surface, cut out of the segment's own
            // baseline-facing edge, so the gap between two fills is exactly one gap
            // wide. A segment too short to give the space keeps its length instead —
            // a sliver that still shows its value beats one eaten by its separator.
            let inner = if (to - from).abs() > gap * 1.5 {
                from + gap * (to - from).signum()
            } else {
                *from
            };
            let rect = bar_rect(orientation, start, size, inner, *to);
            let colour = context.palette.series(data.colour_slot(*series_index));
            marks.push(Mark::Bar(Bar {
                series: *series_index,
                index,
                value: *value,
                radius: if outermost { bar_radius(&rect, context.style.bar_radius) } else { 0.0 },
                rect,
                rounded_end: if outermost {
                    rounded_end(orientation, *positive)
                } else {
                    BarEnd::None
                },
                colour,
            }));

            let series_len = data.series()[*series_index].len();
            let extremes = data.series()[*series_index].extreme_indices();
            if labelled(context.style.labels, index, series_len, extremes) {
                label_bar(
                    context,
                    *series_index,
                    index,
                    *value,
                    &rect,
                    orientation,
                    colour,
                    !outermost,
                );
            }
        }
    }
    marks
}

/// Offers a bar its value label.
///
/// Outside the value end first — the tip is where the eye already is, and a number
/// there never fights the fill. Inside is the fallback, and only when the text fits
/// with room to breathe. An **interior** stacked segment has no free end at all, so
/// it is offered the inside position only; if the segment is too short, the label is
/// dropped rather than hung in space where it would read as its neighbour's.
#[allow(clippy::too_many_arguments)]
fn label_bar(
    context: &mut PlotContext,
    series: usize,
    index: usize,
    value: f64,
    rect: &Rect,
    orientation: Orientation,
    fill: crate::colour::Colour,
    interior: bool,
) {
    let text = context.format(value);
    let gap = context.style.text_gap;
    let positive = value >= 0.0;
    let ink = context.palette.ink_on(fill);
    let anchor = match (orientation, positive) {
        (Orientation::Vertical, true) => Point::new(rect.centre_x(), rect.top()),
        (Orientation::Vertical, false) => Point::new(rect.centre_x(), rect.bottom()),
        (Orientation::Horizontal, true) => Point::new(rect.right(), rect.centre_y()),
        (Orientation::Horizontal, false) => Point::new(rect.left(), rect.centre_y()),
    };
    let rect = *rect;
    let target = LabelTarget { series, index, value, anchor, ink };

    context.try_label(target, text, move |width, line| {
        let inside = match (orientation, positive) {
            (Orientation::Vertical, true) => {
                Rect::new(rect.centre_x() - width * 0.5, rect.top() + gap, width, line)
            }
            (Orientation::Vertical, false) => {
                Rect::new(rect.centre_x() - width * 0.5, rect.bottom() - gap - line, width, line)
            }
            (Orientation::Horizontal, true) => {
                Rect::new(rect.right() - gap - width, rect.centre_y() - line * 0.5, width, line)
            }
            (Orientation::Horizontal, false) => {
                Rect::new(rect.left() + gap, rect.centre_y() - line * 0.5, width, line)
            }
        };
        // Only offer the inside position when the text genuinely fits the fill with
        // padding on both sides. Anything else clips digits off a number.
        let fits_inside = inside.is_inside(&rect.inset(-0.01, -0.01))
            && rect.width >= width + gap * 2.0
            && rect.height >= line + gap;
        if interior {
            return if fits_inside { vec![(inside, Placement::InsideEnd)] } else { Vec::new() };
        }
        let outside = match (orientation, positive) {
            (Orientation::Vertical, true) => {
                Rect::new(rect.centre_x() - width * 0.5, rect.top() - gap - line, width, line)
            }
            (Orientation::Vertical, false) => {
                Rect::new(rect.centre_x() - width * 0.5, rect.bottom() + gap, width, line)
            }
            (Orientation::Horizontal, true) => {
                Rect::new(rect.right() + gap, rect.centre_y() - line * 0.5, width, line)
            }
            (Orientation::Horizontal, false) => {
                Rect::new(rect.left() - gap - width, rect.centre_y() - line * 0.5, width, line)
            }
        };
        let mut candidates = vec![(outside, Placement::OutsideEnd)];
        if fits_inside {
            candidates.push((inside, Placement::InsideEnd));
        }
        candidates
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chart::{ChartData, ChartKind, ChartSpec, build};
    use crate::data::Series;
    use crate::label::LabelPolicy;
    use crate::style::{ChartStyle, LegendPosition};
    use crate::text::MonoMetrics;

    fn frame() -> Rect {
        Rect::new(0.0, 0.0, 480.0, 320.0)
    }

    fn chart(kind: ChartKind, data: Dataset, style: ChartStyle) -> crate::chart::ChartGeometry {
        build(
            &ChartSpec::new(kind, ChartData::Categorical(data)).with_style(style),
            frame(),
            &MonoMetrics::default(),
        )
    }

    fn bars_of(geometry: &crate::chart::ChartGeometry) -> Vec<&Bar> {
        geometry
            .marks
            .iter()
            .filter_map(|mark| match mark {
                Mark::Bar(bar) => Some(bar),
                _ => None,
            })
            .collect()
    }

    fn quarters() -> Dataset {
        Dataset::new(
            ["Q1", "Q2", "Q3", "Q4"],
            [Series::new("Revenue", [12.0, 30.0, 22.0, 41.0])],
        )
    }

    #[test]
    fn every_bar_starts_at_the_baseline_and_ends_at_its_value() {
        let geometry = chart(ChartKind::bar(), quarters(), ChartStyle::default());
        let scale = geometry.axes[0]
            .ticks
            .iter()
            .map(|tick| tick.position)
            .fold(f32::MIN, f32::max);
        for bar in bars_of(&geometry) {
            // The bottom of every positive bar is the axis minimum's position.
            assert!((bar.rect.bottom() - scale).abs() < 1e-3, "{:?}", bar.rect);
            assert!(bar.rect.height > 0.0);
        }
    }

    #[test]
    fn bars_are_capped_and_the_slack_becomes_air() {
        let style = ChartStyle::default();
        let geometry = chart(ChartKind::bar(), Dataset::single(["only"], [5.0]), style.clone());
        let bar = bars_of(&geometry)[0];
        assert!(
            (bar.rect.width - style.bar_max_thickness).abs() < 1e-4,
            "one bar in a 480px frame must still be {}px, not the whole band",
            style.bar_max_thickness
        );
        assert!(bar.rect.centre_x() > geometry.plot.centre_x() - 1.0);
    }

    #[test]
    fn the_value_end_is_rounded_and_the_baseline_end_is_square() {
        let data = Dataset::new(["up", "down"], [Series::new("s", [10.0, -10.0])]);
        let geometry = chart(ChartKind::bar(), data, ChartStyle::default());
        let bars = bars_of(&geometry);
        assert_eq!(bars[0].rounded_end, BarEnd::Top);
        assert_eq!(bars[1].rounded_end, BarEnd::Bottom);
        assert!(bars.iter().all(|bar| bar.radius > 0.0));

        let horizontal = chart(
            ChartKind::horizontal_bar(),
            Dataset::new(["up", "down"], [Series::new("s", [10.0, -10.0])]),
            ChartStyle::default(),
        );
        let bars = bars_of(&horizontal);
        assert_eq!(bars[0].rounded_end, BarEnd::Right);
        assert_eq!(bars[1].rounded_end, BarEnd::Left);
    }

    #[test]
    fn a_bar_too_short_for_a_full_round_gets_a_smaller_one() {
        let data = Dataset::new(["big", "tiny"], [Series::new("s", [1000.0, 1.0])]);
        let geometry = chart(ChartKind::bar(), data, ChartStyle::default());
        let bars = bars_of(&geometry);
        assert_eq!(bars[0].radius, 4.0);
        assert!(bars[1].radius < 4.0, "a 4px round on a 1px bar is a lozenge");
    }

    #[test]
    fn grouped_series_sit_side_by_side_without_touching() {
        let data = Dataset::new(
            ["Q1", "Q2"],
            [
                Series::new("a", [10.0, 20.0]),
                Series::new("b", [12.0, 18.0]),
                Series::new("c", [8.0, 22.0]),
            ],
        );
        let geometry = chart(ChartKind::bar(), data, ChartStyle::default());
        let mut first: Vec<&Bar> = bars_of(&geometry).into_iter().filter(|b| b.index == 0).collect();
        first.sort_by(|a, b| a.rect.x.total_cmp(&b.rect.x));
        assert_eq!(first.len(), 3);
        for pair in first.windows(2) {
            let gap = pair[1].rect.left() - pair[0].rect.right();
            assert!(gap >= 1.9, "series bars must be separated by bare surface, got {gap}");
        }
    }

    #[test]
    fn stacked_segments_are_separated_by_surface_and_only_the_last_is_rounded() {
        let data = Dataset::new(
            ["Q1"],
            [
                Series::new("a", [30.0]),
                Series::new("b", [30.0]),
                Series::new("c", [30.0]),
            ],
        );
        let geometry = chart(ChartKind::stacked_bar(), data, ChartStyle::default());
        let mut bars = bars_of(&geometry);
        bars.sort_by(|a, b| b.rect.y.total_cmp(&a.rect.y));
        assert_eq!(bars.len(), 3);
        for pair in bars.windows(2) {
            let gap = pair[0].rect.top() - pair[1].rect.bottom();
            assert!((gap - 2.0).abs() < 1e-3, "expected one 2px surface gap, got {gap}");
        }
        assert_eq!(bars[0].rounded_end, BarEnd::None, "the segment on the baseline is square");
        assert_eq!(bars[1].rounded_end, BarEnd::None);
        assert_eq!(bars[2].rounded_end, BarEnd::Top, "only the outermost segment rounds");
    }

    #[test]
    fn a_stack_with_both_signs_grows_in_both_directions_from_zero() {
        let data = Dataset::new(
            ["mixed"],
            [Series::new("up", [8.0]), Series::new("down", [-3.0])],
        );
        let geometry = chart(ChartKind::stacked_bar(), data, ChartStyle::default());
        let baseline = geometry.baseline.expect("an axis spanning zero has a zero rule");
        let bars = bars_of(&geometry);
        let up = bars.iter().find(|b| b.value > 0.0).unwrap();
        let down = bars.iter().find(|b| b.value < 0.0).unwrap();
        assert!(up.rect.bottom() <= baseline.from.y + 1e-3);
        assert!(down.rect.top() >= baseline.from.y - 1e-3);
        assert_eq!(down.rounded_end, BarEnd::Bottom);
    }

    #[test]
    fn a_gap_in_a_series_draws_no_bar_rather_than_a_zero() {
        let data = Dataset::new(
            ["a", "b", "c"],
            [Series::with_gaps("s", [Some(5.0), None, Some(7.0)])],
        );
        let geometry = chart(ChartKind::bar(), data, ChartStyle::default());
        let bars = bars_of(&geometry);
        assert_eq!(bars.len(), 2);
        assert!(bars.iter().all(|bar| bar.index != 1));
    }

    #[test]
    fn every_bar_stays_inside_the_plot() {
        for kind in [ChartKind::bar(), ChartKind::stacked_bar(), ChartKind::horizontal_bar()] {
            let data = Dataset::new(
                ["a", "b", "c"],
                [Series::new("x", [4.0, -9.0, 15.0]), Series::new("y", [7.0, 3.0, -2.0])],
            );
            let geometry = chart(kind, data, ChartStyle::default());
            for bar in bars_of(&geometry) {
                assert!(
                    bar.rect.is_inside(&geometry.plot),
                    "{kind:?}: {:?} escapes {:?}",
                    bar.rect,
                    geometry.plot
                );
            }
        }
    }

    #[test]
    fn value_labels_never_overlap_each_other_or_the_axis() {
        let style = ChartStyle::default()
            .with_labels(LabelPolicy::All)
            .with_legend(LegendPosition::None);
        let data = Dataset::new(
            ["Q1", "Q2", "Q3", "Q4"],
            [Series::new("a", [1200.0, 3000.0, 2200.0, 4100.0]),
             Series::new("b", [1100.0, 2900.0, 2300.0, 3900.0])],
        );
        let geometry = chart(ChartKind::bar(), data, style);
        let boxes: Vec<Rect> = geometry
            .labels
            .iter()
            .map(|label| label.label.rect)
            .chain(geometry.axes.iter().flat_map(|axis| axis.labels().map(|l| l.rect)))
            .collect();
        for (i, a) in boxes.iter().enumerate() {
            for b in &boxes[i + 1..] {
                assert!(!a.intersects(b), "{a:?} overlaps {b:?}");
            }
        }
        assert!(!geometry.labels.is_empty());
    }

    #[test]
    fn a_label_inside_a_fill_takes_ink_that_reads_on_that_fill() {
        let style = ChartStyle::default()
            .with_labels(LabelPolicy::All)
            .with_legend(LegendPosition::None);
        let geometry = chart(ChartKind::bar(), quarters(), style);
        for label in &geometry.labels {
            if label.placement == Placement::InsideEnd {
                let bar = bars_of(&geometry)
                    .into_iter()
                    .find(|bar| bar.index == label.index && bar.series == label.series)
                    .unwrap();
                assert!(
                    label.label.colour.contrast_ratio(bar.colour) >= 3.0,
                    "{} on {:?} is unreadable",
                    label.label.text,
                    bar.colour
                );
            }
        }
    }

    #[test]
    fn an_empty_dataset_draws_no_bars_but_still_has_axes() {
        let geometry = chart(ChartKind::bar(), Dataset::default(), ChartStyle::default());
        assert!(geometry.marks.is_empty());
        assert!(geometry.notes.empty);
        assert_eq!(geometry.axes.len(), 2);
        assert!(!geometry.axes[0].ticks.is_empty());
        assert!(geometry.is_finite());
    }

    #[test]
    fn a_frame_too_small_to_draw_in_produces_nothing_rather_than_nonsense() {
        for size in [0.0_f32, 1.0, 8.0, 20.0] {
            let geometry = build(
                &ChartSpec::new(ChartKind::bar(), ChartData::Categorical(quarters())),
                Rect::new(0.0, 0.0, size, size),
                &MonoMetrics::default(),
            );
            assert!(geometry.is_finite(), "{size}px frame produced non-finite geometry");
            for bar in bars_of(&geometry) {
                assert!(bar.rect.is_finite());
                assert!(bar.rect.width >= 0.0 && bar.rect.height >= 0.0);
            }
        }
    }
}
