//! Pies and donuts.
//!
//! A pie is the least precise chart there is — an angle is harder to compare than a
//! length — so this module's job is to make the one thing it *is* good at work
//! properly: part-to-whole, at a glance, over few slices.
//!
//! Three decisions follow from that:
//!
//! - **Slices are the categories of one series.** A pie has one number per slice.
//!   Where a dataset carries several series, the first is plotted and the rest are
//!   reported in [`BuildNotes::ignored_series`](crate::chart::BuildNotes) — summing
//!   them would answer a question nobody asked.
//! - **Non-positive values are dropped, and counted.** A negative quantity has no
//!   angle. Taking its absolute value would silently misstate every other slice's
//!   share, which is worse than a slice that visibly is not there.
//! - **Labels are percentages.** The share is the only thing a pie communicates; the
//!   underlying value belongs in the tooltip or the table.
//!
//! Slices run **clockwise from twelve o'clock**, in the data's own order. Sorting by
//! size would read better and would also mean the chart no longer matches the table
//! it came from.

use crate::chart::{LabelTarget, PlotContext};
use crate::data::Dataset;
use crate::geom::{Arc, Point, Rect};
use crate::label::{LabelPolicy, Placement};
use crate::mark::{Mark, Slice};

/// A donut's hole, as a fraction of its radius. Below the floor the hole is a dot
/// and the chart is a pie with a defect; above the ceiling the ring is too thin to
/// carry a colour.
const MIN_DONUT_RATIO: f32 = 0.2;
const MAX_DONUT_RATIO: f32 = 0.85;

/// Builds the slices, returning them with the number dropped for being non-positive.
pub(crate) fn build(
    data: &Dataset,
    donut_ratio: f32,
    body: Rect,
    context: &mut PlotContext,
) -> (Vec<Mark>, usize) {
    if data.is_empty() || body.is_empty() {
        return (Vec::new(), 0);
    }
    let Some(series) = data.series().first() else {
        return (Vec::new(), 0);
    };

    let mut dropped = 0;
    let mut slices: Vec<(usize, f64)> = Vec::new();
    for index in 0..data.category_count() {
        match series.value(index) {
            Some(value) if value > 0.0 => slices.push((index, value)),
            Some(_) => dropped += 1,
            None => {}
        }
    }
    let total: f64 = slices.iter().map(|(_, value)| *value).sum();
    if total <= 0.0 {
        return (Vec::new(), dropped);
    }

    // Room for labels comes off the radius, not out of the plot: a pie that fills
    // its box and hangs its labels over the edge is the usual way this goes wrong.
    let label_room = if context.style.labels == LabelPolicy::None {
        0.0
    } else {
        context.metrics.line_height(context.style.text_size) + context.style.text_gap * 2.0
    };
    let outer = (body.width.min(body.height) * 0.5 - label_room).max(0.0);
    if outer <= 0.0 {
        return (Vec::new(), dropped);
    }
    let inner = if donut_ratio <= 0.0 {
        0.0
    } else {
        outer * donut_ratio.clamp(MIN_DONUT_RATIO, MAX_DONUT_RATIO)
    };
    let centre = body.centre();

    let mut marks = Vec::with_capacity(slices.len());
    let mut angle = 0.0_f32;
    for (index, value) in slices {
        let share = value / total;
        // The sweep is computed from the share rather than accumulated from the
        // previous angle, so rounding cannot leave a hairline wedge of background
        // between the last slice and the first.
        let sweep = (share as f32) * std::f32::consts::TAU;
        let arc = Arc {
            centre,
            inner_radius: inner,
            outer_radius: outer,
            start_angle: angle,
            end_angle: angle + sweep,
        };
        angle += sweep;

        let colour = context.palette.series(index);
        marks.push(Mark::Slice(Slice {
            series: index,
            index,
            value,
            share,
            arc,
            colour,
        }));

        if context.style.labels != LabelPolicy::None {
            label_slice(context, index, value, share, &arc, colour);
        }
    }
    (marks, dropped)
}

/// Offers a slice its percentage label: inside when the wedge is genuinely wide
/// enough to hold it, otherwise just outside the rim.
///
/// The width test is the chord across the slice at the label's own radius. Without
/// it, a 2% slice gets a label that overflows into its neighbours and reads as
/// theirs — the failure this whole crate's label handling exists to prevent.
fn label_slice(
    context: &mut PlotContext,
    index: usize,
    value: f64,
    share: f64,
    arc: &Arc,
    colour: crate::colour::Colour,
) {
    let text = context.format(share);
    let gap = context.style.text_gap;
    let ink = context.palette.ink_on(colour);
    let inside_anchor = arc.anchor(0.6);
    let outside_anchor = arc.point_at(arc.mid_angle(), arc.outer_radius);
    let chord = 2.0 * (arc.inner_radius + (arc.outer_radius - arc.inner_radius) * 0.6)
        * (arc.sweep() * 0.5).sin().abs();
    let arc = *arc;

    let target = LabelTarget { series: index, index, value, anchor: inside_anchor, ink };
    context.try_label(target, text, move |width, line| {
        let mut candidates = Vec::with_capacity(2);
        if chord >= width + gap * 2.0 && (arc.outer_radius - arc.inner_radius) >= line + gap {
            candidates.push((
                Rect::new(
                    inside_anchor.x - width * 0.5,
                    inside_anchor.y - line * 0.5,
                    width,
                    line,
                ),
                Placement::InsideEnd,
            ));
        }
        // Outside: pushed radially past the rim, and biased left or right by which
        // half of the circle the slice is in so the text runs away from the pie.
        let direction = if arc.mid_angle().sin() >= 0.0 { 1.0 } else { -1.0 };
        let anchor = Point::new(
            outside_anchor.x + direction * gap,
            outside_anchor.y,
        );
        candidates.push((
            Rect::new(
                if direction > 0.0 { anchor.x } else { anchor.x - width },
                anchor.y - line * 0.5,
                width,
                line,
            ),
            Placement::OutsideEnd,
        ));
        candidates
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chart::{ChartData, ChartGeometry, ChartKind, ChartSpec, build as build_chart};
    use crate::style::{ChartStyle, LegendPosition};
    use crate::text::MonoMetrics;
    use std::f32::consts::TAU;

    fn frame() -> Rect {
        Rect::new(0.0, 0.0, 360.0, 320.0)
    }

    fn chart(kind: ChartKind, data: Dataset, style: ChartStyle) -> ChartGeometry {
        build_chart(
            &ChartSpec::new(kind, ChartData::Categorical(data)).with_style(style),
            frame(),
            &MonoMetrics::default(),
        )
    }

    fn slices(geometry: &ChartGeometry) -> Vec<&Slice> {
        geometry
            .marks
            .iter()
            .filter_map(|mark| match mark {
                Mark::Slice(slice) => Some(slice),
                _ => None,
            })
            .collect()
    }

    fn parts() -> Dataset {
        Dataset::single(["Body", "Chassis", "Interior", "Electrical"], [40.0, 30.0, 20.0, 10.0])
    }

    #[test]
    fn slices_fill_the_circle_exactly_once_in_data_order() {
        let geometry = chart(ChartKind::pie(), parts(), ChartStyle::default());
        let slices = slices(&geometry);
        assert_eq!(slices.len(), 4);
        assert_eq!(slices[0].arc.start_angle, 0.0, "the first slice starts at twelve o'clock");
        for pair in slices.windows(2) {
            assert!(
                (pair[0].arc.end_angle - pair[1].arc.start_angle).abs() < 1e-5,
                "slices must not leave a wedge of background between them"
            );
        }
        let total: f32 = slices.iter().map(|slice| slice.arc.sweep()).sum();
        assert!((total - TAU).abs() < 1e-4, "the slices must close the circle: {total}");
        // Shares, not raw values, are what the chart says.
        assert!((slices[0].share - 0.4).abs() < 1e-9);
    }

    #[test]
    fn a_donut_has_a_hole_and_a_pie_does_not() {
        let pie = chart(ChartKind::pie(), parts(), ChartStyle::default());
        assert!(slices(&pie).iter().all(|slice| slice.arc.inner_radius == 0.0));

        let donut = chart(ChartKind::donut(), parts(), ChartStyle::default());
        for slice in slices(&donut) {
            assert!(slice.arc.inner_radius > 0.0);
            assert!(slice.arc.inner_radius < slice.arc.outer_radius);
        }
    }

    #[test]
    fn an_absurd_donut_ratio_is_clamped_to_something_drawable() {
        for ratio in [0.01_f32, 0.99, 5.0] {
            let geometry = chart(ChartKind::Pie { donut_ratio: ratio }, parts(), ChartStyle::default());
            for slice in slices(&geometry) {
                let fraction = slice.arc.inner_radius / slice.arc.outer_radius;
                assert!(
                    (MIN_DONUT_RATIO..=MAX_DONUT_RATIO).contains(&fraction),
                    "ratio {ratio} produced {fraction}"
                );
            }
        }
    }

    #[test]
    fn the_pie_fits_inside_its_frame() {
        let geometry = chart(ChartKind::pie(), parts(), ChartStyle::default());
        for slice in slices(&geometry) {
            let arc = &slice.arc;
            let box_of_circle = Rect::from_edges(
                arc.centre.x - arc.outer_radius,
                arc.centre.y - arc.outer_radius,
                arc.centre.x + arc.outer_radius,
                arc.centre.y + arc.outer_radius,
            );
            assert!(box_of_circle.is_inside(&geometry.frame), "{box_of_circle:?}");
        }
    }

    #[test]
    fn negative_and_zero_values_are_dropped_and_counted() {
        let data = Dataset::single(["a", "b", "c", "d"], [10.0, -5.0, 0.0, 30.0]);
        let geometry = chart(ChartKind::pie(), data, ChartStyle::default());
        assert_eq!(slices(&geometry).len(), 2);
        assert_eq!(geometry.notes.dropped_slices, 2);
        // The dropped values are out of the denominator too, so the rest still add
        // up to the whole circle.
        let total: f32 = slices(&geometry).iter().map(|slice| slice.arc.sweep()).sum();
        assert!((total - TAU).abs() < 1e-4);
    }

    #[test]
    fn a_pie_of_nothing_draws_nothing_and_says_so() {
        for data in [
            Dataset::default(),
            Dataset::single(["a"], [0.0]),
            Dataset::single(["a", "b"], [-1.0, -2.0]),
        ] {
            let geometry = chart(ChartKind::pie(), data, ChartStyle::default());
            assert!(geometry.marks.is_empty());
            assert!(geometry.notes.empty);
            assert!(geometry.is_finite());
        }
    }

    #[test]
    fn one_slice_is_a_whole_circle_rather_than_a_seam() {
        let geometry = chart(ChartKind::pie(), Dataset::single(["all"], [7.0]), ChartStyle::default());
        let slices = slices(&geometry);
        assert_eq!(slices.len(), 1);
        assert!((slices[0].arc.sweep() - TAU).abs() < 1e-5);
        assert!((slices[0].share - 1.0).abs() < 1e-12);
    }

    #[test]
    fn slices_take_colours_by_category_because_the_categories_are_the_series() {
        let palette = crate::palette::Palette::new(crate::palette::Theme::Light);
        let geometry = chart(ChartKind::pie(), parts(), ChartStyle::default());
        for (index, slice) in slices(&geometry).iter().enumerate() {
            assert_eq!(slice.colour, palette.series(index));
        }
        // And the legend keys the categories, not the (single, unnamed) series.
        let legend = geometry.legend.unwrap();
        assert_eq!(legend.entries[0].label.text, "Body");
    }

    #[test]
    fn labels_are_percentages_and_a_thin_slice_puts_its_label_outside() {
        let data = Dataset::single(["big", "sliver"], [99.0, 1.0]);
        let style = ChartStyle::default()
            .with_labels(LabelPolicy::All)
            .with_legend(LegendPosition::None);
        let geometry = chart(ChartKind::pie(), data, style);
        assert_eq!(geometry.labels.len(), 2);
        assert!(geometry.labels.iter().all(|label| label.label.text.ends_with('%')));
        let sliver = geometry.labels.iter().find(|label| label.index == 1).unwrap();
        assert_eq!(sliver.placement, Placement::OutsideEnd);
        assert!(sliver.leader.is_some(), "a label off its slice needs a thread back to it");
    }

    #[test]
    fn pie_labels_never_overlap() {
        let data = Dataset::single(
            ["a", "b", "c", "d", "e", "f"],
            [30.0, 25.0, 20.0, 15.0, 8.0, 2.0],
        );
        let style = ChartStyle::default()
            .with_labels(LabelPolicy::All)
            .with_legend(LegendPosition::None);
        let geometry = chart(ChartKind::pie(), data, style);
        let boxes: Vec<Rect> = geometry.labels.iter().map(|label| label.label.rect).collect();
        for (i, a) in boxes.iter().enumerate() {
            for b in &boxes[i + 1..] {
                assert!(!a.intersects(b), "{a:?} overlaps {b:?}");
            }
        }
    }

    #[test]
    fn a_slice_flattens_into_a_drawable_polygon() {
        let geometry = chart(ChartKind::donut(), parts(), ChartStyle::default());
        for slice in slices(&geometry) {
            let points = slice.arc.flatten(0.25);
            assert!(points.len() > 4);
            assert!(points.iter().all(|point| point.is_finite()));
        }
    }
}
