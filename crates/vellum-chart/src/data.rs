//! The data model: labelled series of numbers, and the aggregates a plot needs.
//!
//! Deliberately small. A chart on a Vellum board is generated from a table
//! (`docs/features/README.md` §2), and a table is a header row plus numbers — there
//! is no query language here, no aggregation, no pivot. What this module *does* own
//! is the boundary where a spreadsheet's mess becomes something geometry can trust.
//!
//! # Missing is not zero
//!
//! A value is `Option<f64>`. A gap in a series means "not measured", and drawing it
//! as zero invents a data point: a line would dive to the axis and back, an area
//! would swallow the hole, and a stacked total would silently shrink. So a gap
//! breaks the line, skips the bar, and is left out of every extent and total.
//!
//! `NaN` and the infinities are folded into that same gap **on entry**, in
//! [`Series::new`] and friends. A spreadsheet column produces them routinely —
//! `0/0`, a text cell, an overflow — and every one of them would otherwise poison a
//! comparison chain and come out the far end as a `NaN` vertex, which does not draw
//! and does not warn. Sanitising once, here, is what lets the rest of the crate
//! assume finiteness instead of checking for it.

use serde::{Deserialize, Serialize};

/// One named row of numbers, aligned to the dataset's categories.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Series {
    pub name: String,
    values: Vec<Option<f64>>,
    slot: Option<usize>,
}

impl Series {
    /// From plain numbers. Anything non-finite becomes a gap.
    pub fn new(name: impl Into<String>, values: impl IntoIterator<Item = f64>) -> Self {
        Self {
            name: name.into(),
            values: values.into_iter().map(sanitise).collect(),
            slot: None,
        }
    }

    /// From values that already carry their own gaps — an imported column where
    /// blank cells are known to be blank rather than zero.
    pub fn with_gaps(
        name: impl Into<String>,
        values: impl IntoIterator<Item = Option<f64>>,
    ) -> Self {
        Self {
            name: name.into(),
            values: values.into_iter().map(|v| v.and_then(sanitise)).collect(),
            slot: None,
        }
    }

    /// Pins this series to a palette slot.
    ///
    /// **Colour follows the entity, not the row number.** By default a series takes
    /// the slot matching its position, which is right until the caller filters the
    /// data: drop the second of four series and the third slides into its colour,
    /// so a reader who learned "Chassis is teal" is now being lied to. A caller that
    /// filters should pin each series to the slot it had in the unfiltered set, and
    /// the survivors keep their colours.
    pub fn with_slot(mut self, slot: usize) -> Self {
        self.slot = Some(slot);
        self
    }

    /// The pinned palette slot, if any.
    pub fn slot(&self) -> Option<usize> {
        self.slot
    }

    /// The value at `index`, or `None` for a gap **or** an index past the end. The
    /// two are the same thing to a plot: a series shorter than the category list
    /// simply has no data for the late categories.
    pub fn value(&self, index: usize) -> Option<f64> {
        self.values.get(index).copied().flatten()
    }

    pub fn len(&self) -> usize {
        self.values.len()
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// The values in order, gaps included.
    pub fn values(&self) -> &[Option<f64>] {
        &self.values
    }

    /// The index of the largest and smallest present value, for the "label the
    /// extremes" policy. `None` when the series is entirely gaps.
    pub fn extreme_indices(&self) -> Option<(usize, usize)> {
        let mut min: Option<(usize, f64)> = None;
        let mut max: Option<(usize, f64)> = None;
        for (index, value) in self.values.iter().enumerate() {
            let Some(value) = value else { continue };
            if min.is_none_or(|(_, m)| *value < m) {
                min = Some((index, *value));
            }
            if max.is_none_or(|(_, m)| *value > m) {
                max = Some((index, *value));
            }
        }
        Some((min?.0, max?.0))
    }
}

fn sanitise(value: f64) -> Option<f64> {
    value.is_finite().then_some(value)
}

/// Series sharing one list of category labels — the shape a table produces, and
/// what bar, line, area and pie charts read.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Dataset {
    categories: Vec<String>,
    series: Vec<Series>,
}

impl Dataset {
    /// Categories are the spine: they fix how many slots the band axis has, and a
    /// series is read against them by position.
    ///
    /// The two lists are reconciled rather than validated. Categories missing for
    /// values that exist get an index label (`"4"`), because a bar with no name is
    /// still a bar the reader can see; values past the last category are dropped,
    /// because there is nowhere on the axis to put them. Returning an error instead
    /// would mean a user's typo produces no chart at all, which is a worse answer
    /// than a chart with one oddly-named column.
    pub fn new(
        categories: impl IntoIterator<Item = impl Into<String>>,
        series: impl IntoIterator<Item = Series>,
    ) -> Self {
        let mut categories: Vec<String> = categories.into_iter().map(Into::into).collect();
        let series: Vec<Series> = series.into_iter().collect();
        let widest = series.iter().map(Series::len).max().unwrap_or(0);
        for index in categories.len()..widest {
            categories.push((index + 1).to_string());
        }
        Self { categories, series }
    }

    /// A single unnamed series — the common case for a pie, and for the one-series
    /// bar chart that needs no legend because the title already names it.
    pub fn single(
        categories: impl IntoIterator<Item = impl Into<String>>,
        values: impl IntoIterator<Item = f64>,
    ) -> Self {
        Self::new(categories, [Series::new("", values)])
    }

    pub fn categories(&self) -> &[String] {
        &self.categories
    }

    pub fn series(&self) -> &[Series] {
        &self.series
    }

    pub fn category_count(&self) -> usize {
        self.categories.len()
    }

    pub fn series_count(&self) -> usize {
        self.series.len()
    }

    /// True when there is nothing to plot: no categories, no series, or every value
    /// a gap. Every builder short-circuits on this, and the empty case is a chart
    /// with axes and no marks rather than a panic or a blank rect.
    pub fn is_empty(&self) -> bool {
        self.categories.is_empty()
            || self.series.is_empty()
            || self.series.iter().all(|s| s.values().iter().all(Option::is_none))
    }

    /// Smallest and largest value present, ignoring gaps. `None` when empty.
    pub fn extent(&self) -> Option<(f64, f64)> {
        let mut extent: Option<(f64, f64)> = None;
        for series in &self.series {
            for index in 0..self.categories.len() {
                let Some(value) = series.value(index) else { continue };
                extent = Some(match extent {
                    None => (value, value),
                    Some((lo, hi)) => (lo.min(value), hi.max(value)),
                });
            }
        }
        extent
    }

    /// The extent a **stacked** plot needs.
    ///
    /// Positive and negative values stack in opposite directions from zero, so the
    /// extent is the largest positive sum against the largest negative sum, not the
    /// sum of everything. A category holding `+8` and `-3` reaches from -3 to +8;
    /// summing to +5 would clip the bar that is actually drawn.
    pub fn stacked_extent(&self) -> Option<(f64, f64)> {
        let mut extent: Option<(f64, f64)> = None;
        for index in 0..self.categories.len() {
            let (mut positive, mut negative) = (0.0_f64, 0.0_f64);
            let mut seen = false;
            for series in &self.series {
                let Some(value) = series.value(index) else { continue };
                seen = true;
                if value >= 0.0 {
                    positive += value;
                } else {
                    negative += value;
                }
            }
            if !seen {
                continue;
            }
            extent = Some(match extent {
                None => (negative, positive),
                Some((lo, hi)) => (lo.min(negative), hi.max(positive)),
            });
        }
        extent
    }

    /// The sum of a category's values, ignoring gaps — the denominator for a pie
    /// and the total a stacked bar's end label carries.
    pub fn category_total(&self, index: usize) -> f64 {
        self.series.iter().filter_map(|s| s.value(index)).sum()
    }

    /// The palette slot series `index` draws in: its pin, or its position. See
    /// [`Series::with_slot`].
    pub fn colour_slot(&self, index: usize) -> usize {
        self.series.get(index).and_then(Series::slot).unwrap_or(index)
    }

    /// Every series' slot, in order — what the legend keys against.
    pub fn colour_slots(&self) -> Vec<usize> {
        (0..self.series.len()).map(|index| self.colour_slot(index)).collect()
    }
}

/// One named cloud of `(x, y)` points — what a scatter plots, and the only data in
/// the crate whose x is a number rather than a label.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct PointSeries {
    pub name: String,
    points: Vec<[f64; 2]>,
    slot: Option<usize>,
}

impl PointSeries {
    /// Points with a non-finite coordinate are dropped whole: unlike a series value,
    /// half a point has nowhere to go — there is no "gap" position on a free x axis.
    pub fn new(name: impl Into<String>, points: impl IntoIterator<Item = [f64; 2]>) -> Self {
        Self {
            name: name.into(),
            points: points
                .into_iter()
                .filter(|[x, y]| x.is_finite() && y.is_finite())
                .collect(),
            slot: None,
        }
    }

    /// Pins this series to a palette slot — see [`Series::with_slot`].
    pub fn with_slot(mut self, slot: usize) -> Self {
        self.slot = Some(slot);
        self
    }

    pub fn slot(&self) -> Option<usize> {
        self.slot
    }

    pub fn points(&self) -> &[[f64; 2]] {
        &self.points
    }

    pub fn len(&self) -> usize {
        self.points.len()
    }

    pub fn is_empty(&self) -> bool {
        self.points.is_empty()
    }
}

/// Point series sharing two numeric axes.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct PointSet {
    series: Vec<PointSeries>,
}

impl PointSet {
    pub fn new(series: impl IntoIterator<Item = PointSeries>) -> Self {
        Self { series: series.into_iter().collect() }
    }

    pub fn series(&self) -> &[PointSeries] {
        &self.series
    }

    pub fn series_count(&self) -> usize {
        self.series.len()
    }

    pub fn is_empty(&self) -> bool {
        self.series.iter().all(PointSeries::is_empty)
    }

    /// The palette slot series `index` draws in: its pin, or its position.
    pub fn colour_slot(&self, index: usize) -> usize {
        self.series.get(index).and_then(PointSeries::slot).unwrap_or(index)
    }

    /// Every series' slot, in order.
    pub fn colour_slots(&self) -> Vec<usize> {
        (0..self.series.len()).map(|index| self.colour_slot(index)).collect()
    }

    /// `((x_min, x_max), (y_min, y_max))`, or `None` when there are no points.
    pub fn extent(&self) -> Option<((f64, f64), (f64, f64))> {
        let mut x: Option<(f64, f64)> = None;
        let mut y: Option<(f64, f64)> = None;
        for series in &self.series {
            for &[px, py] in series.points() {
                x = Some(match x {
                    None => (px, px),
                    Some((lo, hi)) => (lo.min(px), hi.max(px)),
                });
                y = Some(match y {
                    None => (py, py),
                    Some((lo, hi)) => (lo.min(py), hi.max(py)),
                });
            }
        }
        Some((x?, y?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_finite_input_becomes_a_gap_rather_than_a_number() {
        let series = Series::new("s", [1.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY, 2.0]);
        assert_eq!(series.values(), &[Some(1.0), None, None, None, Some(2.0)]);
        let with_gaps = Series::with_gaps("s", [Some(1.0), None, Some(f64::NAN)]);
        assert_eq!(with_gaps.values(), &[Some(1.0), None, None]);
    }

    #[test]
    fn a_series_shorter_than_the_categories_reads_as_gaps_past_its_end() {
        let series = Series::new("s", [1.0, 2.0]);
        assert_eq!(series.value(1), Some(2.0));
        assert_eq!(series.value(2), None);
        assert_eq!(series.value(usize::MAX), None);
    }

    #[test]
    fn missing_category_labels_are_filled_in_by_index() {
        let data = Dataset::new(["Q1"], [Series::new("s", [1.0, 2.0, 3.0])]);
        assert_eq!(data.categories(), &["Q1".to_string(), "2".to_string(), "3".to_string()]);
    }

    #[test]
    fn values_past_the_last_category_are_not_plotted() {
        let data = Dataset::new(["only"], [Series::new("s", [1.0])]);
        let mut long = data.clone();
        long.series[0] = Series::new("s", [1.0, 900.0]);
        // `new` would have grown the categories; a hand-built mismatch must not.
        assert_eq!(long.extent(), Some((1.0, 1.0)));
    }

    #[test]
    fn gaps_are_left_out_of_the_extent() {
        let data = Dataset::new(
            ["a", "b", "c"],
            [Series::with_gaps("s", [Some(5.0), None, Some(-2.0)])],
        );
        assert_eq!(data.extent(), Some((-2.0, 5.0)));
    }

    #[test]
    fn an_all_gap_dataset_is_empty_and_has_no_extent() {
        let data = Dataset::new(["a", "b"], [Series::with_gaps("s", [None, None])]);
        assert!(data.is_empty());
        assert_eq!(data.extent(), None);
        assert_eq!(data.stacked_extent(), None);
        assert_eq!(Dataset::default().extent(), None);
    }

    /// The case that makes `stacked_extent` its own function: a category with values
    /// on both sides of zero reaches further than their sum.
    #[test]
    fn stacking_measures_each_sign_from_zero_separately() {
        let data = Dataset::new(
            ["mixed"],
            [Series::new("up", [8.0]), Series::new("down", [-3.0])],
        );
        assert_eq!(data.stacked_extent(), Some((-3.0, 8.0)));
        assert_eq!(data.category_total(0), 5.0);
    }

    #[test]
    fn stacked_extent_sums_same_sign_values() {
        let data = Dataset::new(
            ["a", "b"],
            [
                Series::new("one", [3.0, -1.0]),
                Series::new("two", [4.0, -2.0]),
                Series::with_gaps("three", [None, Some(-3.0)]),
            ],
        );
        assert_eq!(data.stacked_extent(), Some((-6.0, 7.0)));
    }

    #[test]
    fn extreme_indices_find_the_labelled_points() {
        let series = Series::with_gaps("s", [Some(4.0), None, Some(-1.0), Some(9.0)]);
        assert_eq!(series.extreme_indices(), Some((2, 3)));
        assert_eq!(Series::with_gaps("s", [None]).extreme_indices(), None);
    }

    #[test]
    fn a_scatter_point_with_one_bad_coordinate_is_dropped_whole() {
        let series = PointSeries::new("s", [[1.0, 2.0], [f64::NAN, 3.0], [4.0, f64::INFINITY]]);
        assert_eq!(series.points(), &[[1.0, 2.0]]);
    }

    #[test]
    fn point_set_extent_covers_both_axes_or_neither() {
        let set = PointSet::new([
            PointSeries::new("a", [[0.0, 10.0], [5.0, -2.0]]),
            PointSeries::new("b", [[-3.0, 1.0]]),
        ]);
        assert_eq!(set.extent(), Some(((-3.0, 5.0), (-2.0, 10.0))));
        assert_eq!(PointSet::default().extent(), None);
        assert!(PointSet::new([PointSeries::new("empty", [])]).is_empty());
    }
}
