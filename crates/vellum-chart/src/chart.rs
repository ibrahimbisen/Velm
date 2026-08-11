//! The chart specification, and the one function that turns it into geometry.
//!
//! # Layout happens once, in a fixed order
//!
//! The awkward part of laying out a chart is circular: the plot rectangle depends on
//! how wide the axis labels are, the labels depend on the tick values, the tick
//! values depend on how many ticks fit, and *that* depends on the plot rectangle.
//! Iterating to a fixed point would make the result depend on how many rounds were
//! run. Instead the cycle is cut once, in this order:
//!
//! 1. **The legend** is measured from the series names alone — it never depends on
//!    the plot — and its band comes off the frame.
//! 2. **Tick counts** are estimated from what is left. A target is a hint: asking
//!    for five and getting six changes nothing, so an estimate is enough.
//! 3. **The scales** are built from those targets and then frozen.
//! 4. **The axis bands** are measured from the frozen scales' own labels — the exact
//!    strings that will be drawn.
//! 5. **The plot rectangle** is what remains, and everything is positioned in it.
//!
//! Only step 2 uses an approximation, and it feeds a hint. Every measurement that
//! decides a *position* is made on the final strings, so nothing is laid out against
//! a value it does not end up using.
//!
//! # Nothing here can fail
//!
//! [`build`] returns geometry, never an error. Charts are built from user tables and
//! the interesting inputs are the broken ones — an empty column, one row, every
//! value identical, a category with no name. Each has a defined outcome, and an
//! empty chart still comes back with axes so it reads as "no data" rather than as a
//! crash. What could not be drawn is reported in [`BuildNotes`], which is the honest
//! half of that promise: the caller can see that six labels were dropped or that a
//! seventh series ran past the palette.

use crate::axis::{Axis, AxisSide, band_height, category_band_width, value_band_width};
use crate::bars;
use crate::colour::Colour;
use crate::data::{Dataset, PointSet};
use crate::format::{NumberStyle, format_value};
use crate::geom::{Point, Rect, Segment};
use crate::label::{Label, LabelPolicy, Placement, Placer, TextAlign, ValueLabel};
use crate::legend::Legend;
use crate::lines;
use crate::mark::Mark;
use crate::palette::Palette;
use crate::pies;
use crate::scale::{BandScale, TickOptions, ValueScale};
use crate::style::ChartStyle;
use crate::text::TextMetrics;
use serde::{Deserialize, Serialize};

/// Which way bars run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum Orientation {
    /// Columns growing up from a bottom axis.
    #[default]
    Vertical,
    /// Bars growing right from a left axis. The right choice when category names are
    /// long — a horizontal bar's name has the whole left band to sit in, while a
    /// column's name has only its own column width.
    Horizontal,
}

/// How several series share a category.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum Grouping {
    /// Side by side. Compares series *within* a category.
    #[default]
    Grouped,
    /// End to end. Compares category *totals*, at the cost of every segment except
    /// the first losing its common baseline.
    Stacked,
}

/// The chart forms this crate draws.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum ChartKind {
    Bar { orientation: Orientation, grouping: Grouping },
    Line { markers: bool },
    Area { stacked: bool },
    Scatter,
    /// `donut_ratio` is the hole as a fraction of the radius: 0 for a pie, ~0.6 for
    /// a donut. Clamped to a sane range when it is built.
    Pie { donut_ratio: f32 },
}

impl ChartKind {
    pub fn bar() -> Self {
        Self::Bar { orientation: Orientation::Vertical, grouping: Grouping::Grouped }
    }

    pub fn stacked_bar() -> Self {
        Self::Bar { orientation: Orientation::Vertical, grouping: Grouping::Stacked }
    }

    pub fn horizontal_bar() -> Self {
        Self::Bar { orientation: Orientation::Horizontal, grouping: Grouping::Grouped }
    }

    pub fn line() -> Self {
        Self::Line { markers: true }
    }

    pub fn area() -> Self {
        Self::Area { stacked: false }
    }

    pub fn pie() -> Self {
        Self::Pie { donut_ratio: 0.0 }
    }

    pub fn donut() -> Self {
        Self::Pie { donut_ratio: 0.6 }
    }

    /// Whether a bar-like form's value axis must contain zero. Bars and areas are
    /// read as *lengths from a baseline*, so cropping their axis exaggerates the
    /// difference between them — the single most common way a chart lies. Lines and
    /// scatters are read as shape, and there a cropped axis is the honest way to
    /// show a small variation on a large value.
    fn include_zero(self) -> bool {
        matches!(self, Self::Bar { .. } | Self::Area { .. })
    }

    /// Whether every pair of marks can end up adjacent, which caps how many series
    /// the palette can colour. See [`Palette::SCATTER_CAPACITY`].
    fn compares_all_pairs(self) -> bool {
        matches!(self, Self::Scatter | Self::Pie { .. })
    }
}

/// Either shape of input. The variant decides which axis kinds a chart can have:
/// categorical data has a band axis, points have two numeric ones.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ChartData {
    Categorical(Dataset),
    Points(PointSet),
}

impl ChartData {
    fn series_names(&self, kind: ChartKind) -> Vec<String> {
        match self {
            // A pie's slices are its *categories*, not its series: one column of
            // numbers, split by row. So that is what its legend keys.
            Self::Categorical(data) if matches!(kind, ChartKind::Pie { .. }) => {
                data.categories().to_vec()
            }
            Self::Categorical(data) => data.series().iter().map(|s| s.name.clone()).collect(),
            Self::Points(points) => points.series().iter().map(|s| s.name.clone()).collect(),
        }
    }

    /// How many series the data carries, whichever shape it is.
    pub fn series_count(&self) -> usize {
        match self {
            Self::Categorical(data) => data.series_count(),
            Self::Points(points) => points.series_count(),
        }
    }

    /// The palette slot each legend entry keys, in the same order as
    /// [`Self::series_names`] — pinned slots included, so the legend and the marks
    /// cannot drift apart when a caller filters its data.
    fn colour_slots(&self, kind: ChartKind) -> Vec<usize> {
        match self {
            Self::Categorical(data) if matches!(kind, ChartKind::Pie { .. }) => {
                (0..data.category_count()).collect()
            }
            Self::Categorical(data) => data.colour_slots(),
            Self::Points(points) => points.colour_slots(),
        }
    }
}

/// A chart, before it has a size.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChartSpec {
    pub kind: ChartKind,
    pub data: ChartData,
    pub style: ChartStyle,
}

impl ChartSpec {
    pub fn new(kind: ChartKind, data: ChartData) -> Self {
        Self { kind, data, style: ChartStyle::default() }
    }

    pub fn categorical(kind: ChartKind, data: Dataset) -> Self {
        Self::new(kind, ChartData::Categorical(data))
    }

    pub fn scatter(points: PointSet) -> Self {
        Self::new(ChartKind::Scatter, ChartData::Points(points))
    }

    pub fn with_style(mut self, style: ChartStyle) -> Self {
        self.style = style;
        self
    }
}

/// What the builder could not draw, and why. Never a reason to fail the build — a
/// chart missing four labels is still a chart — but always worth reporting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct BuildNotes {
    /// There was nothing to plot. The axes are still laid out.
    pub empty: bool,
    /// Series past [`Palette::CAPACITY`], all sharing the last slot. Fold them into
    /// a labelled "Other", or facet the chart.
    pub series_over_capacity: usize,
    /// Set on a scatter or pie with more series than
    /// [`Palette::SCATTER_CAPACITY`]: in a form where any two marks can sit side by
    /// side, colour alone stops separating them.
    pub all_pairs_capacity_exceeded: bool,
    /// Value labels that found no room. Their values remain on the axis and in the
    /// caller's table view.
    pub omitted_labels: usize,
    /// Legend entries with no room.
    pub omitted_legend_entries: usize,
    /// Pie slices dropped for having a zero or negative value: a negative quantity
    /// has no angle, and pretending otherwise misstates every other slice's share.
    pub dropped_slices: usize,
    /// Series the form could not use. A pie has one number per slice, so it plots
    /// the first series and reports the rest here rather than summing them.
    pub ignored_series: usize,
}

/// A laid-out chart. Everything is in the caller's coordinate space, inside the
/// frame `build` was given.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChartGeometry {
    pub frame: Rect,
    /// The data area: inside the axes, the legend and the padding.
    pub plot: Rect,
    /// The chart's own background, and the colour of every gap and ring between
    /// marks. The separators are this colour showing through, not paint.
    pub surface: Colour,
    pub axes: Vec<Axis>,
    /// The zero rule, when the value axis crosses zero — one hairline stronger than
    /// the grid, because it is the line the marks are measured from.
    pub baseline: Option<Segment>,
    pub baseline_colour: Colour,
    pub marks: Vec<Mark>,
    pub labels: Vec<ValueLabel>,
    pub legend: Option<Legend>,
    pub notes: BuildNotes,
}

impl ChartGeometry {
    /// Every coordinate in the chart is finite. Cheap enough to assert in a debug
    /// build; the test suite runs it over every chart kind and every pathological
    /// dataset the crate knows about.
    pub fn is_finite(&self) -> bool {
        self.frame.is_finite()
            && self.plot.is_finite()
            && self.marks.iter().all(Mark::is_finite)
            && self.labels.iter().all(|label| label.label.rect.is_finite())
            && self.baseline.is_none_or(|line| line.is_finite())
            && self.axes.iter().all(|axis| {
                axis.ticks.iter().all(|tick| {
                    tick.position.is_finite()
                        && tick.gridline.is_none_or(|line| line.is_finite())
                        && tick.label.as_ref().is_none_or(|label| label.rect.is_finite())
                })
            })
    }
}

/// Everything the per-kind builders share: what to draw with, and where labels have
/// already been put.
pub(crate) struct PlotContext<'a> {
    pub(crate) style: &'a ChartStyle,
    pub(crate) palette: &'a Palette,
    pub(crate) metrics: &'a dyn TextMetrics,
    pub(crate) number_style: NumberStyle,
    /// Decimal places for value labels, taken from the value axis so a label and a
    /// tick that show the same number show it the same way.
    pub(crate) decimals: u8,
    pub(crate) placer: Placer,
    pub(crate) labels: Vec<ValueLabel>,
    pub(crate) omitted_labels: usize,
}

impl<'a> PlotContext<'a> {
    pub(crate) fn new(
        style: &'a ChartStyle,
        palette: &'a Palette,
        metrics: &'a dyn TextMetrics,
        number_style: NumberStyle,
        decimals: u8,
        clip: Rect,
    ) -> Self {
        Self {
            style,
            palette,
            metrics,
            number_style,
            decimals,
            placer: Placer::new(clip, style.label_padding),
            labels: Vec::new(),
            omitted_labels: 0,
        }
    }

    pub(crate) fn format(&self, value: f64) -> String {
        format_value(value, self.decimals, self.number_style)
    }

    /// Offers a label to the placer. `candidates` is given the measured width and
    /// line height and returns boxes in order of preference; the first that fits
    /// wins, and if none does the label is dropped and counted.
    pub(crate) fn try_label(
        &mut self,
        target: LabelTarget,
        text: String,
        candidates: impl Fn(f32, f32) -> Vec<(Rect, Placement)>,
    ) {
        let size = self.style.text_size;
        let width = self.metrics.width(&text, size);
        let line = self.metrics.line_height(size);
        let Some((rect, placement)) = self.placer.place(&candidates(width, line)) else {
            self.omitted_labels += 1;
            return;
        };
        let colour = match placement {
            Placement::InsideEnd => target.ink,
            _ => self.palette.text,
        };
        // A label that had to leave its mark keeps a thread back to it. Nudging
        // labels apart without one detaches them from the data and reads as noise.
        let leader = (target.anchor.distance(rect.centre()) > line * 1.5)
            .then(|| Segment::new(target.anchor, nearest_edge_point(&rect, target.anchor)));
        self.labels.push(ValueLabel {
            series: target.series,
            index: target.index,
            value: target.value,
            label: Label::new(text, rect, TextAlign::Centre, size, colour),
            placement,
            anchor: target.anchor,
            leader,
        });
    }
}

/// What a label is *about*: the mark it names, the point it hangs off, and the ink
/// it takes if it ends up inside the fill.
///
/// `ink` is the one place text follows the data. A label set inside a coloured mark
/// has no surface of its own to sit on, so it takes whichever of the system's ink
/// and paper reads on that fill. Every other label wears a text token.
#[derive(Debug, Clone, Copy)]
pub(crate) struct LabelTarget {
    pub(crate) series: usize,
    pub(crate) index: usize,
    pub(crate) value: f64,
    pub(crate) anchor: Point,
    pub(crate) ink: Colour,
}

/// The point on `rect`'s boundary closest to `from` — where a leader line should
/// meet its label, rather than running to the centre and crossing the text.
fn nearest_edge_point(rect: &Rect, from: Point) -> Point {
    Point::new(
        from.x.clamp(rect.left(), rect.right()),
        from.y.clamp(rect.top(), rect.bottom()),
    )
}

/// Lays out `spec` inside `frame`.
///
/// Every pairing of [`ChartKind`] and [`ChartData`] draws something, including the
/// two that do not really match: point data asked for as a pie draws the points as a
/// scatter, and categorical data asked for as a scatter draws it as a line. Both are
/// a caller mistake, and both produce a readable chart of the data that was actually
/// supplied rather than an empty rectangle.
pub fn build(spec: &ChartSpec, frame: Rect, metrics: &dyn TextMetrics) -> ChartGeometry {
    let style = &spec.style;
    let palette = Palette::new(style.theme);
    let names = spec.data.series_names(spec.kind);
    let slots = spec.data.colour_slots(spec.kind);
    // Capacity is counted over the *slots*, not the series count: a caller that has
    // pinned a series to slot 8 has run past the palette even with three series.
    let mut notes = BuildNotes {
        series_over_capacity: slots.iter().filter(|slot| **slot >= Palette::CAPACITY).count(),
        all_pairs_capacity_exceeded: spec.kind.compares_all_pairs()
            && slots.iter().filter(|slot| **slot < Palette::CAPACITY).count()
                > Palette::SCATTER_CAPACITY,
        ..BuildNotes::default()
    };

    let frame = Rect::new(
        frame.x,
        frame.y,
        frame.width.max(0.0),
        frame.height.max(0.0),
    );
    let content = frame.inset(style.padding, style.padding);

    // 1. The legend, measured from the names alone.
    let legend = Legend::build(&names, &slots, style.legend, content, style, &palette, metrics);
    if let Some(legend) = &legend {
        notes.omitted_legend_entries = legend.omitted;
    }
    let body = match &legend {
        Some(legend) => match legend.position {
            crate::style::LegendPosition::Top => {
                content.inset_sides(0.0, legend.bounds.height + style.text_gap, 0.0, 0.0)
            }
            crate::style::LegendPosition::Bottom => {
                content.inset_sides(0.0, 0.0, 0.0, legend.bounds.height + style.text_gap)
            }
            crate::style::LegendPosition::Left => {
                content.inset_sides(legend.bounds.width + style.text_gap, 0.0, 0.0, 0.0)
            }
            crate::style::LegendPosition::Right => {
                content.inset_sides(0.0, 0.0, legend.bounds.width + style.text_gap, 0.0)
            }
            crate::style::LegendPosition::None => content,
        },
        None => content,
    };

    match (&spec.data, spec.kind) {
        (ChartData::Categorical(data), ChartKind::Pie { donut_ratio }) => {
            let mut context = PlotContext::new(
                style,
                &palette,
                metrics,
                NumberStyle::Percent,
                0,
                frame,
            );
            reserve_legend(&mut context, legend.as_ref());
            let (marks, dropped) = pies::build(data, donut_ratio, body, &mut context);
            notes.empty = marks.is_empty();
            notes.dropped_slices = dropped;
            notes.ignored_series = data.series_count().saturating_sub(1);
            notes.omitted_labels = context.omitted_labels;
            ChartGeometry {
                frame,
                plot: body,
                surface: palette.surface,
                axes: Vec::new(),
                baseline: None,
                baseline_colour: palette.baseline,
                marks,
                labels: context.labels,
                legend,
                notes,
            }
        }
        (ChartData::Points(points), _) => {
            build_scatter(spec, frame, body, points, legend, notes, &palette, metrics)
        }
        (ChartData::Categorical(data), kind) => {
            build_categorical(spec, kind, frame, body, data, legend, notes, &palette, metrics)
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn build_categorical(
    spec: &ChartSpec,
    kind: ChartKind,
    frame: Rect,
    body: Rect,
    data: &Dataset,
    legend: Option<Legend>,
    mut notes: BuildNotes,
    palette: &Palette,
    metrics: &dyn TextMetrics,
) -> ChartGeometry {
    let style = &spec.style;
    let horizontal = matches!(kind, ChartKind::Bar { orientation: Orientation::Horizontal, .. });
    let stacked = matches!(
        kind,
        ChartKind::Bar { grouping: Grouping::Stacked, .. } | ChartKind::Area { stacked: true }
    );

    // 2-3. Tick target from the rough body, then the scale, then freeze it.
    let extent = if stacked { data.stacked_extent() } else { data.extent() };
    let rough = if horizontal { body.width } else { body.height };
    let options = TickOptions {
        target: style.tick_target(rough, !horizontal),
        include_zero: kind.include_zero(),
    };
    let scale = match extent {
        Some((min, max)) => ValueScale::nice(min, max, options),
        None => {
            notes.empty = true;
            ValueScale::fallback()
        }
    };
    let number_style = spec.style.number_style.unwrap_or_else(|| {
        NumberStyle::for_axis(scale.step(), scale.max_magnitude())
    });

    // 4-5. Bands from the frozen scale's own labels, then what remains is the plot.
    let (value_side, category_side) = if horizontal {
        (AxisSide::Bottom, AxisSide::Left)
    } else {
        (AxisSide::Left, AxisSide::Bottom)
    };
    let value_band = if horizontal {
        band_height(style, metrics)
    } else {
        value_band_width(&scale, style, metrics, number_style)
    };
    // A category axis may not eat the plot: a third of the frame is as much as long
    // names get, and past that they are truncated.
    let category_cap = body.width * 0.33;
    let category_band = if horizontal {
        category_band_width(data.categories(), category_cap, style, metrics)
    } else {
        band_height(style, metrics)
    };
    // The end labels of a value axis hang half outside the plot; that overhang is
    // reserved here or it lands on the legend above, or off the frame to the right.
    // A category axis needs none: its labels are centred on band centres, which the
    // band's own outer padding already holds clear of the plot's edges.
    let widest_value = crate::axis::widest_value_label(&scale, style, metrics, number_style);
    let overhang = crate::axis::axis_overhang(value_side, widest_value, style, metrics);
    let plot = if horizontal {
        body.inset_sides(category_band.max(overhang), 0.0, overhang, value_band)
    } else {
        body.inset_sides(value_band, overhang, 0.0, category_band)
    };

    let bands = if horizontal {
        BandScale::new(data.category_count(), plot.top(), plot.bottom())
    } else {
        BandScale::new(data.category_count(), plot.left(), plot.right())
    };

    let mut context = PlotContext::new(
        style,
        palette,
        metrics,
        number_style,
        scale.decimals(),
        frame,
    );

    // Tick labels are not negotiable, so they are reserved before any value label is
    // offered a position — a value must never overprint the axis it is read against.
    let mut value_axis = Axis::value(
        &scale,
        value_side,
        plot,
        style,
        palette,
        metrics,
        number_style,
    );
    let mut category_axis = Axis::category(
        data.categories(),
        &bands,
        category_side,
        plot,
        if horizontal { category_cap } else { bands.pitch() },
        style,
        palette,
        metrics,
    );
    // The two axes meet at a corner, and only one label can have it. The vertical
    // axis keeps its own — see `Axis::drop_labels_colliding_with`.
    if horizontal {
        let reserved: Vec<Rect> = category_axis.labels().map(|label| label.rect).collect();
        value_axis.drop_labels_colliding_with(&reserved, style.label_padding);
    } else {
        let reserved: Vec<Rect> = value_axis.labels().map(|label| label.rect).collect();
        category_axis.drop_labels_colliding_with(&reserved, style.label_padding);
    }
    for label in value_axis.labels().chain(category_axis.labels()) {
        context.placer.reserve(label.rect);
    }
    reserve_legend(&mut context, legend.as_ref());

    let marks = match kind {
        ChartKind::Bar { orientation, grouping } => {
            bars::build(data, &scale, &bands, plot, orientation, grouping, &mut context)
        }
        ChartKind::Line { markers } => {
            lines::build_lines(data, &scale, &bands, plot, markers, &mut context)
        }
        ChartKind::Area { stacked } => {
            lines::build_areas(data, &scale, &bands, plot, stacked, &mut context)
        }
        // A pie never reaches here, and a scatter needs point data; both are routed
        // in `build`. Categorical data in a scatter is a caller error that should
        // still draw something, so it falls back to a line's marks.
        ChartKind::Scatter | ChartKind::Pie { .. } => {
            lines::build_lines(data, &scale, &bands, plot, true, &mut context)
        }
    };

    notes.omitted_labels = context.omitted_labels;
    notes.empty = notes.empty || marks.is_empty();

    let baseline = zero_rule(&scale, plot, horizontal);
    let mut axes = vec![value_axis, category_axis];
    // Where the category rule and the zero rule land on the same line, only the
    // stronger one is drawn: two hairlines a pixel apart read as a mistake.
    if let Some(baseline) = baseline
        && let Some(rule) = axes[1].rule
        && baseline.from.distance(rule.from) < 1.0
        && baseline.to.distance(rule.to) < 1.0
    {
        axes[1].rule = None;
    }

    ChartGeometry {
        frame,
        plot,
        surface: palette.surface,
        axes,
        baseline,
        baseline_colour: palette.baseline,
        marks,
        labels: context.labels,
        legend,
        notes,
    }
}

#[allow(clippy::too_many_arguments)]
fn build_scatter(
    spec: &ChartSpec,
    frame: Rect,
    body: Rect,
    points: &PointSet,
    legend: Option<Legend>,
    mut notes: BuildNotes,
    palette: &Palette,
    metrics: &dyn TextMetrics,
) -> ChartGeometry {
    let style = &spec.style;
    let (x_extent, y_extent) = match points.extent() {
        Some((x, y)) => (x, y),
        None => {
            notes.empty = true;
            ((0.0, 1.0), (0.0, 1.0))
        }
    };
    // Both axes are free to crop: a scatter is read as a shape, and forcing zero
    // onto both axes squashes the cloud into a corner.
    let x_scale = ValueScale::nice(
        x_extent.0,
        x_extent.1,
        TickOptions { target: style.tick_target(body.width, false), include_zero: false },
    );
    let y_scale = ValueScale::nice(
        y_extent.0,
        y_extent.1,
        TickOptions { target: style.tick_target(body.height, true), include_zero: false },
    );
    let x_style = style
        .number_style
        .unwrap_or_else(|| NumberStyle::for_axis(x_scale.step(), x_scale.max_magnitude()));
    let y_style = style
        .number_style
        .unwrap_or_else(|| NumberStyle::for_axis(y_scale.step(), y_scale.max_magnitude()));

    // Both axes overhang: the y axis by half a line at the top, the x axis by half
    // its widest label at each end.
    let x_overhang = crate::axis::widest_value_label(&x_scale, style, metrics, x_style) * 0.5;
    let y_overhang = metrics.line_height(style.text_size) * 0.5;
    let plot = body.inset_sides(
        value_band_width(&y_scale, style, metrics, y_style).max(x_overhang),
        y_overhang,
        x_overhang,
        band_height(style, metrics),
    );

    let mut context =
        PlotContext::new(style, palette, metrics, y_style, y_scale.decimals(), frame);
    let y_axis = Axis::value(&y_scale, AxisSide::Left, plot, style, palette, metrics, y_style);
    let mut x_axis = Axis::value(&x_scale, AxisSide::Bottom, plot, style, palette, metrics, x_style);
    // Two value axes meet at the origin corner, where the x axis's first label
    // reaches back under the y axis's lowest one.
    let reserved: Vec<Rect> = y_axis.labels().map(|label| label.rect).collect();
    x_axis.drop_labels_colliding_with(&reserved, style.label_padding);
    for label in y_axis.labels().chain(x_axis.labels()) {
        context.placer.reserve(label.rect);
    }
    reserve_legend(&mut context, legend.as_ref());

    let marks = lines::build_scatter(points, &x_scale, &y_scale, plot, &mut context);
    notes.omitted_labels = context.omitted_labels;
    notes.empty = notes.empty || marks.is_empty();

    ChartGeometry {
        frame,
        plot,
        surface: palette.surface,
        axes: vec![y_axis, x_axis],
        baseline: None,
        baseline_colour: palette.baseline,
        marks,
        labels: context.labels,
        legend,
        notes,
    }
}

/// Legend keys are text a value label must not overprint, exactly like tick labels.
fn reserve_legend(context: &mut PlotContext, legend: Option<&Legend>) {
    let Some(legend) = legend else { return };
    for entry in &legend.entries {
        context.placer.reserve(entry.label.rect);
        context.placer.reserve(entry.swatch);
    }
}

/// The zero rule, when the axis crosses zero.
fn zero_rule(scale: &ValueScale, plot: Rect, horizontal: bool) -> Option<Segment> {
    if !scale.contains_zero() {
        return None;
    }
    Some(if horizontal {
        Segment::vertical(
            scale.position(0.0, plot.left(), plot.right()),
            plot.top(),
            plot.bottom(),
        )
    } else {
        Segment::horizontal(
            scale.position(0.0, plot.bottom(), plot.top()),
            plot.left(),
            plot.right(),
        )
    })
}

/// The value-label policy applied to one series, resolved once so the per-kind
/// builders do not each re-derive it.
pub(crate) fn labelled(
    policy: LabelPolicy,
    index: usize,
    len: usize,
    extremes: Option<(usize, usize)>,
) -> bool {
    policy.labels(index, len, extremes)
}
