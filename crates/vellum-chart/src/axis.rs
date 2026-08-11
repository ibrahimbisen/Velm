//! Axes: the ticks a scale produces, turned into positioned labels and gridlines.
//!
//! [`crate::scale`] decides *which* numbers an axis carries. This module decides
//! where they go, which of them survive, and how much of the frame the axis costs —
//! and that last part is why an axis is measured before the plot rectangle exists.
//! A left axis is exactly as wide as its widest label plus a 4px gap: reserving a
//! fixed width would either waste the plot's space or clip `-1,250`.
//!
//! # Thinning, not shrinking
//!
//! When labels do not fit, the labels go, not the ticks. The gridlines stay where
//! the scale put them, and every *n*th one keeps its number — an axis reading
//! `0, 20, 40` with hairlines at every 10 is still a scale, while an axis whose
//! labels were shrunk to 7px is unreadable and one whose ticks were resampled is a
//! different scale. [`crate::label::choose_stride`] picks *n*, preferring strides
//! that keep both ends of the axis, since the ends are where a reader looks for the
//! range.
//!
//! # Chrome recedes
//!
//! Gridlines and rules are `frost`, one step off the surface, 1px, solid. Never
//! dashed: dashing reads as "projection" or "threshold" when all it means is grid.
//! The zero rule is the one line allowed to be stronger, and the chart builder emits
//! it separately — it is the line every bar is measured from, and it is worth more
//! than a gridline exactly once per chart.

use crate::colour::Colour;
use crate::format::{NumberStyle, format_value};
use crate::geom::{Rect, Segment};
use crate::label::{Label, TextAlign, choose_stride};
use crate::palette::Palette;
use crate::scale::{BandScale, ValueScale};
use crate::style::ChartStyle;
use crate::text::{TextMetrics, truncate_to_width};
use serde::{Deserialize, Serialize};

/// Which edge of the plot an axis runs along.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AxisSide {
    Left,
    Right,
    Top,
    Bottom,
}

impl AxisSide {
    /// True when the axis line itself is vertical, which is what decides whether its
    /// labels stack (and crowd on height) or sit side by side (and crowd on width).
    pub fn is_vertical(self) -> bool {
        matches!(self, Self::Left | Self::Right)
    }
}

/// What an axis is measuring.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AxisRole {
    /// Numbers, from a [`ValueScale`].
    Value,
    /// Names, from a [`BandScale`].
    Category,
}

/// One position on an axis.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tick {
    /// The data value for a value axis, or the category index for a band axis.
    pub value: f64,
    /// Where it sits, in chart space: an x for a horizontal axis, a y for a vertical
    /// one.
    pub position: f32,
    /// `None` when the label was thinned out or would not fit.
    pub label: Option<Label>,
    /// The hairline across the plot. Category axes have none — a line per category
    /// is a cage, not a grid.
    pub gridline: Option<Segment>,
}

/// A laid-out axis.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Axis {
    pub side: AxisSide,
    pub role: AxisRole,
    /// The rule along the axis itself. Present on the axis the marks stand on, absent
    /// on the one the gridlines already draw.
    pub rule: Option<Segment>,
    pub ticks: Vec<Tick>,
    pub colour: Colour,
}

impl Axis {
    /// A numeric axis over `plot`.
    ///
    /// `plot` must be the final plot rectangle: everything here is positioned
    /// against it. The band this axis needs was already measured by
    /// [`value_band_width`] or [`band_height`] before the rectangle was carved out.
    pub fn value(
        scale: &ValueScale,
        side: AxisSide,
        plot: Rect,
        style: &ChartStyle,
        palette: &Palette,
        metrics: &dyn TextMetrics,
        number_style: NumberStyle,
    ) -> Self {
        // A plot with no room draws nothing, labels included. Ticks laid out against
        // a collapsed rectangle would all land on the same pixel and their labels
        // would pile up outside a chart that has no space for them.
        if plot.is_empty() {
            return Self { side, role: AxisRole::Value, rule: None, ticks: Vec::new(), colour: palette.grid };
        }
        let size = style.text_size;
        let line = metrics.line_height(size);
        let texts: Vec<String> = scale
            .ticks()
            .iter()
            .map(|value| format_value(*value, scale.decimals(), number_style))
            .collect();
        let positions: Vec<f32> = scale
            .ticks()
            .iter()
            .map(|value| position_on(scale, *value, side, plot))
            .collect();

        // Thin against the direction the labels actually crowd in.
        let extents: Vec<(f32, f32)> = positions
            .iter()
            .zip(&texts)
            .map(|(position, text)| {
                if side.is_vertical() {
                    (position - line * 0.5, position + line * 0.5)
                } else {
                    let half = metrics.width(text, size) * 0.5;
                    (position - half, position + half)
                }
            })
            .collect();
        let stride = choose_stride(&extents, style.text_gap);

        let ticks = texts
            .into_iter()
            .zip(positions)
            .enumerate()
            .map(|(index, (text, position))| {
                let label = index.is_multiple_of(stride).then(|| {
                    let width = metrics.width(&text, size);
                    let rect = label_rect(side, position, width, line, plot, style);
                    Label::new(text, rect, label_align(side), size, palette.text_muted)
                });
                Tick {
                    value: scale.ticks()[index],
                    position,
                    label,
                    gridline: style.gridlines.then(|| gridline(side, position, plot)),
                }
            })
            .collect();

        Self { side, role: AxisRole::Value, rule: None, ticks, colour: palette.grid }
    }

    /// A category axis over `plot`, one tick per band centre.
    ///
    /// `budget` caps a label's width; on a left axis it is the band reserved for the
    /// axis, and on a bottom axis it is the band pitch, so a name is truncated to
    /// the column it belongs to rather than sprawling across its neighbours.
    #[allow(clippy::too_many_arguments)]
    pub fn category(
        labels: &[String],
        bands: &BandScale,
        side: AxisSide,
        plot: Rect,
        budget: f32,
        style: &ChartStyle,
        palette: &Palette,
        metrics: &dyn TextMetrics,
    ) -> Self {
        if plot.is_empty() {
            return Self {
                side,
                role: AxisRole::Category,
                rule: None,
                ticks: Vec::new(),
                colour: palette.grid,
            };
        }
        let size = style.text_size;
        let line = metrics.line_height(size);
        let count = bands.count().min(labels.len());

        let placed: Vec<(f32, Option<String>)> = (0..count)
            .map(|index| {
                // `bands` was built over whichever axis this is, so its centres are
                // already x for a bottom axis and y for a left one.
                let position = bands.centre(index);
                (position, truncate_to_width(&labels[index], size, budget, metrics))
            })
            .collect();

        let extents: Vec<(f32, f32)> = placed
            .iter()
            .map(|(position, text)| {
                let extent = match (side.is_vertical(), text) {
                    (true, _) => line,
                    (false, Some(text)) => metrics.width(text, size),
                    (false, None) => 0.0,
                };
                (position - extent * 0.5, position + extent * 0.5)
            })
            .collect();
        let stride = choose_stride(&extents, style.text_gap);

        let ticks = placed
            .into_iter()
            .enumerate()
            .map(|(index, (position, text))| {
                let label = text.filter(|_| index.is_multiple_of(stride)).map(|text| {
                    let width = metrics.width(&text, size);
                    let rect = label_rect(side, position, width, line, plot, style);
                    Label::new(text, rect, label_align(side), size, palette.text_muted)
                });
                Tick { value: index as f64, position, label, gridline: None }
            })
            .collect();

        Self {
            side,
            role: AxisRole::Category,
            rule: Some(rule_for(side, plot)),
            ticks,
            colour: palette.grid,
        }
    }

    /// The labels that survived thinning.
    pub fn labels(&self) -> impl Iterator<Item = &Label> {
        self.ticks.iter().filter_map(|tick| tick.label.as_ref())
    }

    /// Drops the labels that would overprint `reserved`, returning how many went.
    ///
    /// The corner where two axes meet is the one place their labels can collide: a
    /// bottom axis centres its first label on the plot's left edge, and that box
    /// reaches back into the left axis's band. Nothing about thinning catches it,
    /// because within each axis the labels are perfectly spaced.
    ///
    /// The left axis wins. It carries the scale the marks are *measured* against,
    /// while the label it displaces is one end of the other axis's range — and the
    /// tick, the gridline and the axis rule all stay, so the position is still
    /// readable.
    pub fn drop_labels_colliding_with(&mut self, reserved: &[Rect], padding: f32) -> usize {
        let mut dropped = 0;
        for tick in &mut self.ticks {
            let Some(label) = &tick.label else { continue };
            let padded = label.rect.inset(-padding, -padding);
            if reserved.iter().any(|other| other.intersects(&padded)) {
                tick.label = None;
                dropped += 1;
            }
        }
        dropped
    }
}

/// Where a value sits along `side` of `plot`.
fn position_on(scale: &ValueScale, value: f64, side: AxisSide, plot: Rect) -> f32 {
    if side.is_vertical() {
        // y grows downwards, so the domain's minimum is at the plot's *bottom*.
        scale.position(value, plot.bottom(), plot.top())
    } else {
        scale.position(value, plot.left(), plot.right())
    }
}

/// The hairline a tick draws across the plot, perpendicular to its own axis.
fn gridline(side: AxisSide, position: f32, plot: Rect) -> Segment {
    if side.is_vertical() {
        Segment::horizontal(position, plot.left(), plot.right())
    } else {
        Segment::vertical(position, plot.top(), plot.bottom())
    }
}

/// The rule along the axis itself.
fn rule_for(side: AxisSide, plot: Rect) -> Segment {
    match side {
        AxisSide::Left => Segment::vertical(plot.left(), plot.top(), plot.bottom()),
        AxisSide::Right => Segment::vertical(plot.right(), plot.top(), plot.bottom()),
        AxisSide::Top => Segment::horizontal(plot.top(), plot.left(), plot.right()),
        AxisSide::Bottom => Segment::horizontal(plot.bottom(), plot.left(), plot.right()),
    }
}

/// A tick label's box: flush to the axis, centred on the tick.
fn label_rect(
    side: AxisSide,
    position: f32,
    width: f32,
    line: f32,
    plot: Rect,
    style: &ChartStyle,
) -> Rect {
    let gap = style.text_gap;
    match side {
        AxisSide::Left => Rect::new(plot.left() - gap - width, position - line * 0.5, width, line),
        AxisSide::Right => Rect::new(plot.right() + gap, position - line * 0.5, width, line),
        AxisSide::Top => Rect::new(position - width * 0.5, plot.top() - gap - line, width, line),
        AxisSide::Bottom => Rect::new(position - width * 0.5, plot.bottom() + gap, width, line),
    }
}

/// Which edge of a tick label stays put if a renderer measures the text slightly
/// differently: the one against the axis.
fn label_align(side: AxisSide) -> TextAlign {
    match side {
        AxisSide::Left => TextAlign::End,
        AxisSide::Right => TextAlign::Start,
        AxisSide::Top | AxisSide::Bottom => TextAlign::Centre,
    }
}

/// The widest label this scale will draw.
///
/// Measured from the scale rather than from a guess, and measured *before* the plot
/// rectangle is decided — which is the whole reason these are free functions and not
/// methods on a built axis.
pub fn widest_value_label(
    scale: &ValueScale,
    style: &ChartStyle,
    metrics: &dyn TextMetrics,
    number_style: NumberStyle,
) -> f32 {
    scale
        .ticks()
        .iter()
        .map(|value| metrics.width(&format_value(*value, scale.decimals(), number_style), style.text_size))
        .fold(0.0_f32, f32::max)
}

/// How wide a vertical value axis needs to be: its widest label, plus the gap to
/// the plot.
pub fn value_band_width(
    scale: &ValueScale,
    style: &ChartStyle,
    metrics: &dyn TextMetrics,
    number_style: NumberStyle,
) -> f32 {
    widest_value_label(scale, style, metrics, number_style) + style.text_gap
}

/// How far an axis's end labels stick out **past** the plot, along the axis.
///
/// A tick label is centred on its tick, and the first and last ticks sit exactly on
/// the plot's corners — so half of each end label hangs outside the plot entirely.
/// On a vertical axis that is half a line of height above the top; on a horizontal
/// one it is half the widest label's width past each end. Nothing else reserves it,
/// and unreserved it is what makes the top tick label collide with the legend and
/// the last one run off the frame.
pub fn axis_overhang(side: AxisSide, widest_label: f32, style: &ChartStyle, metrics: &dyn TextMetrics) -> f32 {
    if side.is_vertical() {
        metrics.line_height(style.text_size) * 0.5
    } else {
        widest_label * 0.5
    }
}

/// How tall a horizontal axis of single-line labels needs to be.
pub fn band_height(style: &ChartStyle, metrics: &dyn TextMetrics) -> f32 {
    metrics.line_height(style.text_size) + style.text_gap
}

/// How wide a vertical *category* axis needs to be, given a cap on how much of the
/// frame it may take. Long names are truncated rather than allowed to eat the plot.
pub fn category_band_width(
    labels: &[String],
    cap: f32,
    style: &ChartStyle,
    metrics: &dyn TextMetrics,
) -> f32 {
    let widest = labels
        .iter()
        .map(|label| metrics.width(label, style.text_size))
        .fold(0.0_f32, f32::max);
    widest.min(cap) + style.text_gap
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::palette::Theme;
    use crate::scale::TickOptions;
    use crate::text::MonoMetrics;

    fn plot() -> Rect {
        Rect::new(60.0, 20.0, 300.0, 200.0)
    }

    fn value_axis(scale: &ValueScale, side: AxisSide, style: &ChartStyle) -> Axis {
        Axis::value(
            scale,
            side,
            plot(),
            style,
            &Palette::new(Theme::Light),
            &MonoMetrics::default(),
            NumberStyle::Plain,
        )
    }

    #[test]
    fn a_left_axis_puts_its_minimum_at_the_bottom_of_the_plot() {
        let scale = ValueScale::nice(0.0, 100.0, TickOptions::FROM_ZERO);
        let axis = value_axis(&scale, AxisSide::Left, &ChartStyle::default());
        let first = &axis.ticks[0];
        let last = axis.ticks.last().unwrap();
        assert_eq!(first.value, 0.0);
        assert!((first.position - plot().bottom()).abs() < 1e-4);
        assert!((last.position - plot().top()).abs() < 1e-4);
    }

    #[test]
    fn tick_labels_sit_outside_the_plot_flush_to_their_axis() {
        let scale = ValueScale::nice(0.0, 100.0, TickOptions::FROM_ZERO);
        let style = ChartStyle::default();
        let axis = value_axis(&scale, AxisSide::Left, &style);
        for label in axis.labels() {
            assert!((label.rect.right() - (plot().left() - style.text_gap)).abs() < 1e-4);
            assert_eq!(label.align, TextAlign::End);
        }
        let bottom = value_axis(&scale, AxisSide::Bottom, &style);
        for label in bottom.labels() {
            assert!(label.rect.top() >= plot().bottom());
            assert_eq!(label.align, TextAlign::Centre);
        }
    }

    #[test]
    fn gridlines_span_the_plot_and_follow_the_gridline_switch() {
        let scale = ValueScale::nice(0.0, 100.0, TickOptions::FROM_ZERO);
        let axis = value_axis(&scale, AxisSide::Left, &ChartStyle::default());
        for tick in &axis.ticks {
            let line = tick.gridline.unwrap();
            assert!((line.from.x - plot().left()).abs() < 1e-4);
            assert!((line.to.x - plot().right()).abs() < 1e-4);
            assert_eq!(line.from.y, tick.position);
        }
        let bare = ChartStyle { gridlines: false, ..ChartStyle::default() };
        assert!(value_axis(&scale, AxisSide::Left, &bare).ticks.iter().all(|t| t.gridline.is_none()));
    }

    /// Ticks are never resampled to make labels fit — the grid keeps its spacing and
    /// only the numbers thin out.
    #[test]
    fn a_cramped_axis_thins_labels_and_keeps_every_tick() {
        let scale = ValueScale::nice(0.0, 1_000_000.0, TickOptions::FROM_ZERO.with_target(12));
        let tiny = Rect::new(60.0, 20.0, 90.0, 200.0);
        let axis = Axis::value(
            &scale,
            AxisSide::Bottom,
            tiny,
            &ChartStyle::default(),
            &Palette::new(Theme::Light),
            &MonoMetrics::default(),
            NumberStyle::Plain,
        );
        assert_eq!(axis.ticks.len(), scale.ticks().len());
        assert!(axis.labels().count() < axis.ticks.len(), "labels should have thinned");
        // What survives does not overlap.
        let boxes: Vec<Rect> = axis.labels().map(|l| l.rect).collect();
        for (i, a) in boxes.iter().enumerate() {
            for b in &boxes[i + 1..] {
                assert!(!a.intersects(b), "{a:?} overlaps {b:?}");
            }
        }
    }

    #[test]
    fn the_reserved_band_matches_the_widest_label_that_gets_drawn() {
        let scale = ValueScale::nice(-1250.0, 1250.0, TickOptions::FREE);
        let style = ChartStyle::default();
        let metrics = MonoMetrics::default();
        let band = value_band_width(&scale, &style, &metrics, NumberStyle::Plain);
        let axis = value_axis(&scale, AxisSide::Left, &style);
        for label in axis.labels() {
            assert!(
                label.rect.width + style.text_gap <= band + 1e-4,
                "{} needs {} of a {band} band",
                label.text,
                label.rect.width
            );
        }
    }

    #[test]
    fn a_category_axis_centres_names_under_their_bands_and_carries_a_rule() {
        let labels: Vec<String> = ["Q1", "Q2", "Q3", "Q4"].iter().map(|s| s.to_string()).collect();
        let bands = BandScale::new(4, plot().left(), plot().right());
        let axis = Axis::category(
            &labels,
            &bands,
            AxisSide::Bottom,
            plot(),
            bands.pitch(),
            &ChartStyle::default(),
            &Palette::new(Theme::Light),
            &MonoMetrics::default(),
        );
        assert_eq!(axis.ticks.len(), 4);
        assert!(axis.rule.is_some());
        for (index, tick) in axis.ticks.iter().enumerate() {
            assert!((tick.position - bands.centre(index)).abs() < 1e-4);
            assert!(tick.gridline.is_none(), "a line per category is a cage, not a grid");
            let label = tick.label.as_ref().unwrap();
            assert!((label.rect.centre_x() - tick.position).abs() < 1e-4);
        }
    }

    #[test]
    fn a_name_too_long_for_its_column_is_truncated_not_sprawled() {
        let labels = vec!["Rear subframe assembly".to_string(), "B".to_string()];
        let bands = BandScale::new(2, plot().left(), plot().right());
        let axis = Axis::category(
            &labels,
            &bands,
            AxisSide::Bottom,
            plot(),
            40.0,
            &ChartStyle::default(),
            &Palette::new(Theme::Light),
            &MonoMetrics::default(),
        );
        let first = axis.ticks[0].label.as_ref().unwrap();
        assert!(first.text.ends_with('…'), "{}", first.text);
        assert!(first.rect.width <= 40.0);
    }

    #[test]
    fn an_axis_over_an_empty_band_scale_produces_no_ticks_rather_than_panicking() {
        let bands = BandScale::new(0, plot().left(), plot().right());
        let axis = Axis::category(
            &[],
            &bands,
            AxisSide::Bottom,
            plot(),
            50.0,
            &ChartStyle::default(),
            &Palette::new(Theme::Light),
            &MonoMetrics::default(),
        );
        assert!(axis.ticks.is_empty());
    }

    #[test]
    fn every_axis_number_is_finite() {
        for (min, max) in [(0.0, 1.0), (-1e9, 1e9), (0.0, 0.0), (f64::NAN, 3.0)] {
            let scale = ValueScale::nice(min, max, TickOptions::FREE);
            for side in [AxisSide::Left, AxisSide::Bottom, AxisSide::Right, AxisSide::Top] {
                let axis = value_axis(&scale, side, &ChartStyle::default());
                for tick in &axis.ticks {
                    assert!(tick.position.is_finite());
                    assert!(tick.gridline.is_none_or(|line| line.is_finite()));
                    assert!(tick.label.as_ref().is_none_or(|l| l.rect.is_finite()));
                }
            }
        }
    }
}
