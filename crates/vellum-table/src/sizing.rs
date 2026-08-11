//! Track sizing: how wide a column is, how tall a row is, and what a resize costs.
//!
//! # The three modes
//!
//! [`Sizing::Fixed`] is a width the user set. [`Sizing::Auto`] fits the content.
//! [`Sizing::Proportional`] takes a share of whatever is left over once the other
//! two have been paid.
//!
//! # Auto means *max-content*, clamped — not "whatever fits"
//!
//! This is the single most consequential decision in the crate, so it is worth
//! stating plainly. An auto column sizes to the **widest unwrapped line** in it,
//! clamped to `[min, max]`. It does **not** depend on the width available to the
//! table.
//!
//! The tempting alternative — auto-fit that takes the available width into account,
//! as CSS's real table algorithm does — makes column width a function of the
//! table's width, and row height a function of column width, and the table's height
//! a function of that. Solving it is a fixed-point iteration, and every column
//! resize re-enters it for the whole table. That is precisely the quadratic reflow
//! `docs/01-architecture.md` §3 exists to avoid, and it is why Miro's own tables
//! stutter on a large board.
//!
//! Making auto width-independent buys three things:
//!
//! - A column's intrinsic size is a pure function of its cells' content, so it is
//!   cacheable on cell identity alone and **survives every resize**.
//! - Resizing column *j* re-measures the cells in column *j* and nothing else. See
//!   [`MeasureCache`](crate::MeasureCache), whose statistics let a test assert it.
//! - The layout is stable: dragging a column and dragging it back returns exactly
//!   the widths you started with, because nothing depends on the path taken.
//!
//! What it costs is a column that could have been narrower if the table were
//! squeezed. That is what `min` and `max` are for, and what
//! [`Fit::Width`] does afterwards.
//!
//! # Why the defaults are asymmetric
//!
//! An auto **column** caps at [`MAX_AUTO_COLUMN_WIDTH`]; an auto **row** has no cap
//! at all. Uncapped auto-fit turns one long sentence into a 3000px column, and the
//! remedy — wrapping — is free and invisible. There is no equivalent remedy
//! vertically: a row that refused to grow would hide its own text, and the text has
//! nowhere else to go.

use serde::{Deserialize, Serialize};

use crate::geometry::{EPSILON, sanitise};

/// The narrowest an auto column gets, however empty. A column narrower than this is
/// hard to click into and reads as a rendering fault rather than as a column.
pub const MIN_COLUMN_WIDTH: f64 = 48.0;

/// Where auto-fit stops and wrapping starts. See the module docs for the asymmetry
/// with rows.
pub const MAX_AUTO_COLUMN_WIDTH: f64 = 480.0;

/// The shortest an auto row gets — one line of 13px text plus the default 8px
/// padding, rounded onto the 4px grid.
pub const MIN_ROW_HEIGHT: f64 = 24.0;

/// The floor an *interactive* resize clamps to.
///
/// Deliberately far below [`MIN_COLUMN_WIDTH`]: the model allows a 4px spacer
/// column, because a user may genuinely want one, and only the drag gesture is
/// clamped. The reason it is clamped at all is that a zero-width column has no
/// boundary a cursor can distinguish from its neighbour's, so dragging one to zero
/// would be a one-way door.
pub const MIN_TRACK_SIZE: f64 = 8.0;

/// How a column's width or a row's height is decided.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum Sizing {
    /// Exactly this many world px. What an interactive resize produces.
    Fixed(f64),
    /// Fit the content: max-content, clamped to `[min, max]`. `max: None` is
    /// uncapped, which is the right default for a row and the wrong one for a
    /// column.
    Auto { min: f64, max: Option<f64> },
    /// Take `weight`-proportional share of the space left after the fixed and auto
    /// tracks are paid, never dropping below `min`.
    ///
    /// Only meaningful for columns, and only under [`Fit::Width`]: a table's height
    /// is always its content's, so a proportional row behaves as [`Auto`] with the
    /// same `min`.
    ///
    /// [`Auto`]: Sizing::Auto
    Proportional { weight: f64, min: f64 },
}

impl Default for Sizing {
    fn default() -> Self {
        Self::Auto { min: 0.0, max: None }
    }
}

impl Sizing {
    /// The default for a column: auto-fit, floored and capped.
    pub const fn auto_column() -> Self {
        Self::Auto { min: MIN_COLUMN_WIDTH, max: Some(MAX_AUTO_COLUMN_WIDTH) }
    }

    /// The default for a row: auto-fit, floored and uncapped.
    pub const fn auto_row() -> Self {
        Self::Auto { min: MIN_ROW_HEIGHT, max: None }
    }

    pub fn is_auto(self) -> bool {
        matches!(self, Self::Auto { .. })
    }

    pub fn is_fixed(self) -> bool {
        matches!(self, Self::Fixed(_))
    }

    pub fn is_proportional(self) -> bool {
        matches!(self, Self::Proportional { .. })
    }

    /// The size below which this track will not be squeezed.
    pub fn floor(self) -> f64 {
        match self {
            Self::Fixed(size) => sanitise(size),
            Self::Auto { min, .. } | Self::Proportional { min, .. } => sanitise(min),
        }
    }

    /// The size before any distribution of leftover space.
    ///
    /// `natural` reports whether the table is sizing to its content
    /// ([`Fit::Natural`]). It only changes the proportional case, and it has to:
    /// with no target width there is no leftover to take a share of, so a
    /// proportional track that reported its bare `min` would make `Fit::Natural`
    /// produce a table no `Fit::Width` could reproduce.
    fn base_size(self, intrinsic: TrackIntrinsic, natural: bool) -> f64 {
        match self {
            Self::Fixed(size) => sanitise(size),
            Self::Auto { min, max } => {
                let fitted = sanitise(intrinsic.max_content).max(sanitise(min));
                match max {
                    Some(max) => fitted.min(sanitise(max)),
                    None => fitted,
                }
            }
            Self::Proportional { min, .. } => {
                let min = sanitise(min);
                if natural { sanitise(intrinsic.max_content).max(min) } else { min }
            }
        }
    }
}

/// What a track's content needs, in world px.
///
/// `min_content` is the widest thing that cannot be broken — the longest word, in
/// practice. A track narrower than that will have text sticking out of it, which is
/// why [`CellLayout::content_overflows`](crate::CellLayout::content_overflows)
/// exists rather than the layout silently clipping.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct TrackIntrinsic {
    pub min_content: f64,
    pub max_content: f64,
}

impl TrackIntrinsic {
    pub fn new(min_content: f64, max_content: f64) -> Self {
        Self { min_content: sanitise(min_content), max_content: sanitise(max_content) }
    }

    /// Folds another cell's requirement in: a track must satisfy all of its cells,
    /// so both figures are maxima.
    pub fn absorb(&mut self, other: Self) {
        self.min_content = self.min_content.max(other.min_content);
        self.max_content = self.max_content.max(other.max_content);
    }
}

/// How much room the table has to lay out in.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub enum Fit {
    /// The table takes its content's width. What an unconstrained widget on a board
    /// does.
    #[default]
    Natural,
    /// The table is stretched or squeezed to exactly this width, if it has any
    /// flexible track to do it with. What dragging the table's own right edge does.
    Width(f64),
}

/// Resolves every track's base size from its sizing and its content.
pub(crate) fn base_sizes(sizings: &[Sizing], intrinsic: &[TrackIntrinsic], fit: Fit) -> Vec<f64> {
    let natural = matches!(fit, Fit::Natural);
    sizings
        .iter()
        .enumerate()
        .map(|(i, sizing)| sizing.base_size(intrinsic.get(i).copied().unwrap_or_default(), natural))
        .collect()
}

/// Widens the tracks a merged cell spans until they can hold it.
///
/// **Only `Auto` tracks in the span absorb the deficit.** A `Fixed` track is a width
/// the user set, and quietly widening it because some cell three rows down spans
/// across it would be a table that reflows when you were not looking. If a span
/// crosses no auto track there is nowhere for the deficit to go, and the cell
/// reports [`content_overflows`](crate::CellLayout::content_overflows) instead.
///
/// The deficit is shared in proportion to current size, so a span across a wide and
/// a narrow column keeps their relationship rather than levelling them.
pub(crate) fn distribute_span_requirement(
    sizes: &mut [f64],
    sizings: &[Sizing],
    start: usize,
    count: usize,
    required: f64,
) {
    let end = (start + count).min(sizes.len());
    if start >= end {
        return;
    }
    let current: f64 = sizes[start..end].iter().sum();
    let deficit = required - current;
    if deficit <= EPSILON {
        return;
    }
    let targets: Vec<usize> = (start..end).filter(|&i| sizings[i].is_auto()).collect();
    if targets.is_empty() {
        return;
    }
    let total: f64 = targets.iter().map(|&i| sizes[i]).sum();
    if total <= EPSILON {
        let share = deficit / targets.len() as f64;
        for &i in &targets {
            sizes[i] += share;
        }
    } else {
        for &i in &targets {
            sizes[i] += deficit * sizes[i] / total;
        }
    }
}

/// Stretches or squeezes the tracks to hit `target`, returning the width actually
/// achieved.
///
/// The flexible set is the proportional tracks if there are any, and the auto tracks
/// otherwise. A table of nothing but fixed columns keeps its own width and reports
/// it — the alternative, overriding the widths the user pinned, is worse than a
/// table that does not fill its frame.
///
/// An auto track's `max` is deliberately **not** applied here. That cap exists to
/// stop auto-*fit* running away with one long sentence; it is not a statement that
/// the user may never drag the column wider.
pub(crate) fn fit_to_width(sizes: &mut [f64], sizings: &[Sizing], target: f64) -> f64 {
    let target = sanitise(target);
    let total: f64 = sizes.iter().sum();
    let mut remaining = target - total;
    if remaining.abs() <= EPSILON {
        return total;
    }

    let mut active: Vec<usize> =
        (0..sizes.len()).filter(|&i| sizings[i].is_proportional()).collect();
    if active.is_empty() {
        active = (0..sizes.len()).filter(|&i| sizings[i].is_auto()).collect();
    }
    if active.is_empty() {
        return total;
    }

    // Weights are taken once, from the base sizes. Recomputing them as the sizes
    // move would make the result depend on the order tracks clamp in.
    let weights: Vec<f64> = sizes
        .iter()
        .enumerate()
        .map(|(i, &size)| match sizings[i] {
            Sizing::Proportional { weight, .. } => sanitise(weight),
            _ => size.max(1.0),
        })
        .collect();

    // Shrinking can push a track under its floor, at which point it stops taking a
    // share and the rest is redistributed. Each pass retires at least one track, so
    // this runs at most `active.len()` times.
    while remaining.abs() > EPSILON && !active.is_empty() {
        let total_weight: f64 = active.iter().map(|&i| weights[i]).sum();
        if total_weight <= EPSILON {
            break;
        }
        let mut clamped: Vec<usize> = Vec::new();
        let mut consumed = 0.0;
        for &i in &active {
            let share = remaining * weights[i] / total_weight;
            let floor = sizings[i].floor();
            if sizes[i] + share < floor {
                consumed += floor - sizes[i];
                sizes[i] = floor;
                clamped.push(i);
            } else {
                sizes[i] += share;
                consumed += share;
            }
        }
        remaining -= consumed;
        if clamped.is_empty() {
            break;
        }
        active.retain(|i| !clamped.contains(i));
    }

    sizes.iter().sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn intrinsics(pairs: &[(f64, f64)]) -> Vec<TrackIntrinsic> {
        pairs.iter().map(|&(min, max)| TrackIntrinsic::new(min, max)).collect()
    }

    #[test]
    fn auto_fits_content_between_its_floor_and_its_cap() {
        let sizings = [
            Sizing::Auto { min: 48.0, max: Some(480.0) },
            Sizing::Auto { min: 48.0, max: Some(480.0) },
            Sizing::Auto { min: 48.0, max: Some(480.0) },
        ];
        let content = intrinsics(&[(10.0, 10.0), (10.0, 200.0), (900.0, 900.0)]);
        let sizes = base_sizes(&sizings, &content, Fit::Natural);
        assert_eq!(sizes, vec![48.0, 200.0, 480.0]);
    }

    /// An auto track is a pure function of its content, which is the property the
    /// whole incremental story rests on.
    #[test]
    fn auto_sizing_does_not_depend_on_the_available_width() {
        let sizings = [Sizing::auto_column()];
        let content = intrinsics(&[(60.0, 120.0)]);
        let natural = base_sizes(&sizings, &content, Fit::Natural);
        let squeezed = base_sizes(&sizings, &content, Fit::Width(60.0));
        let stretched = base_sizes(&sizings, &content, Fit::Width(2000.0));
        assert_eq!(natural, squeezed);
        assert_eq!(natural, stretched);
    }

    #[test]
    fn proportional_tracks_split_the_leftover_by_weight() {
        let sizings = [
            Sizing::Fixed(100.0),
            Sizing::Proportional { weight: 1.0, min: 0.0 },
            Sizing::Proportional { weight: 3.0, min: 0.0 },
        ];
        let content = intrinsics(&[(0.0, 0.0); 3]);
        let mut sizes = base_sizes(&sizings, &content, Fit::Width(500.0));
        let total = fit_to_width(&mut sizes, &sizings, 500.0);
        assert_eq!(sizes, vec![100.0, 100.0, 300.0]);
        assert_eq!(total, 500.0);
    }

    /// Squeezing has to stop somewhere, and what stops it must not be the last track
    /// in index order taking the whole loss.
    #[test]
    fn squeezing_respects_every_floor_and_redistributes_the_rest() {
        let sizings = [
            Sizing::Proportional { weight: 1.0, min: 90.0 },
            Sizing::Proportional { weight: 1.0, min: 10.0 },
        ];
        let mut sizes = vec![100.0, 100.0];
        let total = fit_to_width(&mut sizes, &sizings, 120.0);
        assert_eq!(
            sizes,
            vec![90.0, 30.0],
            "the first hits its floor, the second absorbs the rest"
        );
        assert_eq!(total, 120.0);
    }

    #[test]
    fn a_table_of_fixed_columns_keeps_its_own_width() {
        let sizings = [Sizing::Fixed(100.0), Sizing::Fixed(140.0)];
        let mut sizes = vec![100.0, 140.0];
        assert_eq!(fit_to_width(&mut sizes, &sizings, 1000.0), 240.0);
        assert_eq!(sizes, vec![100.0, 140.0]);
    }

    /// Without proportional tracks, dragging the table's edge scales the auto ones —
    /// otherwise the gesture would do nothing on the common table.
    #[test]
    fn auto_tracks_take_the_stretch_when_nothing_is_proportional() {
        let sizings = [Sizing::auto_column(), Sizing::auto_column()];
        let mut sizes = vec![100.0, 300.0];
        fit_to_width(&mut sizes, &sizings, 800.0);
        assert_eq!(sizes, vec![200.0, 600.0], "scaled in proportion, not levelled");
    }

    #[test]
    fn a_span_widens_only_the_auto_tracks_it_crosses() {
        let sizings = [Sizing::Fixed(100.0), Sizing::auto_column(), Sizing::auto_column()];
        let mut sizes = vec![100.0, 50.0, 150.0];
        distribute_span_requirement(&mut sizes, &sizings, 0, 3, 400.0);
        assert_eq!(sizes[0], 100.0, "a fixed column is not widened behind the user's back");
        assert_eq!(sizes[1] + sizes[2], 300.0);
        assert!(sizes[2] > sizes[1], "shared in proportion to current size");
    }

    #[test]
    fn a_span_across_only_fixed_tracks_leaves_them_alone() {
        let sizings = [Sizing::Fixed(40.0), Sizing::Fixed(40.0)];
        let mut sizes = vec![40.0, 40.0];
        distribute_span_requirement(&mut sizes, &sizings, 0, 2, 400.0);
        assert_eq!(sizes, vec![40.0, 40.0]);
    }

    #[test]
    fn a_span_that_already_fits_is_left_alone() {
        let sizings = [Sizing::auto_column(), Sizing::auto_column()];
        let mut sizes = vec![100.0, 100.0];
        distribute_span_requirement(&mut sizes, &sizings, 0, 2, 150.0);
        assert_eq!(sizes, vec![100.0, 100.0]);
    }

    #[test]
    fn degenerate_sizings_collapse_rather_than_producing_nan() {
        let sizings = [Sizing::Fixed(f64::NAN), Sizing::Auto { min: -10.0, max: Some(f64::NAN) }];
        let content = intrinsics(&[(0.0, 0.0), (0.0, 50.0)]);
        let sizes = base_sizes(&sizings, &content, Fit::Natural);
        assert!(sizes.iter().all(|s| s.is_finite() && *s >= 0.0));
    }
}
