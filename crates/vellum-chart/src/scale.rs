//! Scales: the map from data to pixels, and the tick values that map lands on.
//!
//! # Nice numbers, and why they are the whole ballgame
//!
//! An axis drawn straight from the data reads `0, 23.7, 47.4, 71.1, 94.8`. An axis
//! drawn properly reads `0, 25, 50, 75, 100`. The data is identical; the second one
//! is a chart and the first one is output. Nothing else in a chart gives it away
//! faster, which is why this module is the most heavily tested in the crate.
//!
//! The algorithm is Heckbert's (*Graphics Gems*, 1990) nice-number rule: a step is
//! only ever **1, 2, 2.5 or 5** times a power of ten, because those are the
//! intervals whose multiples a reader can add up without thinking. The raw step —
//! the span divided by roughly how many labels are wanted — is rounded to the
//! nearest such number, and then the domain is grown outward to whole multiples of
//! it. That growth is the part people leave out: without it the first tick is not a
//! nice number, only the spacing is.
//!
//! The 2.5 is not decoration. Heckbert's original set is 1-2-5, and on `0..94.8`
//! with five labels wanted it produces `0, 20, 40, 60, 80, 100` — correct, but not
//! what anyone would draw by hand. Quartering is how people actually divide a
//! hundred, and it needs a step of 25. Spreadsheets have carried 2.5 for the same
//! reason for thirty years.
//!
//! # Degenerate input is the normal case
//!
//! Charts are built from user tables, so the interesting inputs are the broken ones:
//! an empty column, one row, a column of identical values, everything negative, a
//! range that straddles zero, and magnitudes at both ends of what an `f64` holds.
//! Every one of them must produce a readable axis. None of them may produce `NaN`,
//! an empty tick list, or a loop. [`ValueScale::nice`] is total — there is no error
//! path, because there is no input for which "no axis" is a better answer than a
//! sensible default one.
//!
//! Floating point is handled with a deliberate epsilon rather than by hoping: with
//! `step = 0.1`, `-0.3 / 0.1` is `-3.0000000000000004`, whose floor is `-4`, which
//! would put a spurious tick at `-0.4` and shift the whole axis. Every division that
//! feeds a `floor` or `ceil` here goes through [`floor_with_tolerance`] or its twin.

use crate::format::{MAX_DECIMALS, decimals_for_step};
use serde::{Deserialize, Serialize};

/// What the caller wants from an axis, before the data is looked at.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct TickOptions {
    /// How many labels the axis has room for. A *target*, not a promise: rounding
    /// the step to 1, 2 or 5 means the count lands near it, and a nice axis with six
    /// ticks beats an ugly one with exactly five.
    pub target: usize,
    /// Whether zero must be inside the domain.
    ///
    /// True for anything drawn from a baseline — a bar's length only means something
    /// measured from zero, and an axis starting at 90 makes a 2% difference look
    /// like a doubling. False for lines and scatters, where the reader is comparing
    /// shape rather than magnitude and a cropped axis is the honest way to show a
    /// small variation on a large value.
    pub include_zero: bool,
}

impl TickOptions {
    /// For value axes drawn from a baseline: bar charts and area charts.
    pub const FROM_ZERO: Self = Self { target: 5, include_zero: true };
    /// For value axes that may crop: line and scatter.
    pub const FREE: Self = Self { target: 5, include_zero: false };

    pub const fn with_target(self, target: usize) -> Self {
        Self { target, ..self }
    }
}

impl Default for TickOptions {
    fn default() -> Self {
        Self::FROM_ZERO
    }
}

/// A linear axis: a domain rounded outwards to whole steps, and the ticks in it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ValueScale {
    min: f64,
    max: f64,
    step: f64,
    decimals: u8,
    ticks: Vec<f64>,
}

/// No axis gets more ticks than this. Reached only by pathological input — a step
/// that underflows, or a domain spanning most of the `f64` range — and the cap is
/// what turns "the loop runs for a week" into "the axis looks coarse".
const MAX_TICKS: usize = 101;

/// The most labels any real axis wants. A 4000px axis at the design language's 11px
/// label size could hold more, but past this the labels stop being read and start
/// being texture.
const MAX_TARGET: usize = 40;

impl ValueScale {
    /// Builds a nice axis over `[data_min, data_max]`.
    ///
    /// Total by construction. Non-finite bounds, a reversed pair, an empty range and
    /// a range too wide to subtract all resolve to a sensible domain rather than an
    /// error, and each resolution is asserted in this module's tests.
    pub fn nice(data_min: f64, data_max: f64, options: TickOptions) -> Self {
        let (mut min, mut max) = sanitise_domain(data_min, data_max);

        if options.include_zero {
            min = min.min(0.0);
            max = max.max(0.0);
        }

        // A single point, or a column of identical values. There is no span to
        // divide, so one is invented: the value sits in the middle of a domain half
        // its own magnitude either side, which keeps the ticks in the same order of
        // magnitude as the data. All-zero data has no magnitude to work from and
        // gets the unit interval.
        if min == max {
            let magnitude = min.abs();
            if magnitude == 0.0 {
                min = 0.0;
                max = 1.0;
            } else {
                let half = magnitude * 0.5;
                min -= half;
                max += half;
            }
        }

        let target = options.target.clamp(2, MAX_TARGET);
        let mut step = nice_step((max - min) / (target - 1) as f64);

        // An `f64` at magnitude *m* resolves to about *m·ε*, so near 1e16 the gap
        // between representable numbers is 2. A step finer than that produces
        // consecutive ticks that are *the same number* — an axis with a repeated
        // label and a zero-width interval between two gridlines drawn on top of each
        // other. Growing the step to the next nice value above the resolution is the
        // only honest answer: at that magnitude the data has fewer distinct values
        // than the axis was asked to show.
        let resolution = min.abs().max(max.abs()) * f64::EPSILON;
        if step <= resolution * 4.0 {
            step = nice_step(resolution * 8.0);
        }

        // `step` is positive and finite here, but the domain may still be wide
        // enough that whole multiples of it overflow the tick budget. Growing the
        // step by decades converges in a handful of turns and keeps the 1-2-5
        // property, where clamping the tick count would not.
        let (mut nice_min, mut nice_max) = (min, max);
        for _ in 0..MAX_DECIMALS {
            nice_min = floor_with_tolerance(min / step) * step;
            nice_max = ceil_with_tolerance(max / step) * step;
            let spans = ((nice_max - nice_min) / step).round();
            if spans.is_finite() && spans < MAX_TICKS as f64 {
                break;
            }
            step *= 10.0;
        }

        // Rounding outwards can still leave a degenerate domain if the step
        // underflowed to zero against a huge magnitude — 1e300 with a step of 1 puts
        // both ends on the same representable value.
        if !nice_min.is_finite() || !nice_max.is_finite() || nice_max <= nice_min {
            return Self::fallback();
        }

        let decimals = decimals_for_step(step);
        let count = (((nice_max - nice_min) / step).round() as usize + 1).min(MAX_TICKS);
        let ticks: Vec<f64> = (0..count)
            .map(|index| {
                // Computed from the origin rather than accumulated, so error does
                // not grow along the axis, then snapped to the step's own precision
                // so the label and the position agree exactly.
                snap_to_decimals(nice_min + index as f64 * step, decimals)
            })
            .collect();

        // The domain's ends *are* its first and last tick. Snapping them the same
        // way keeps `min()` and `ticks().first()` the same number, which everything
        // downstream — gridline positions, the baseline, label formatting — assumes.
        let nice_min = ticks.first().copied().unwrap_or(nice_min);
        let nice_max = ticks.last().copied().unwrap_or(nice_max);

        Self { min: nice_min, max: nice_max, step, decimals, ticks }
    }

    /// The axis used when there is nothing to draw: `0..1` in quarters. Chosen over
    /// an empty axis because an empty chart with an axis reads as "no data yet",
    /// while a chart with no axis at all reads as broken.
    pub fn fallback() -> Self {
        Self::nice(0.0, 1.0, TickOptions { target: 5, include_zero: true })
    }

    pub fn min(&self) -> f64 {
        self.min
    }

    pub fn max(&self) -> f64 {
        self.max
    }

    /// The gap between ticks. Always positive and finite.
    pub fn step(&self) -> f64 {
        self.step
    }

    /// How many decimal places every label on this axis carries.
    pub fn decimals(&self) -> u8 {
        self.decimals
    }

    pub fn ticks(&self) -> &[f64] {
        &self.ticks
    }

    pub fn span(&self) -> f64 {
        self.max - self.min
    }

    pub fn contains_zero(&self) -> bool {
        self.min <= 0.0 && self.max >= 0.0
    }

    /// The largest magnitude on the axis, for choosing a number style.
    pub fn max_magnitude(&self) -> f64 {
        self.min.abs().max(self.max.abs())
    }

    /// Maps a value onto the pixel interval `[start, end]`.
    ///
    /// `end` may be *less* than `start`, and for a vertical axis it always is: y
    /// grows downwards, so the top of the plot is the smaller number and the caller
    /// passes `(bottom, top)`. Values outside the domain map outside the interval
    /// rather than being clamped — a caller that wants clipping knows where its plot
    /// rectangle is, and silently pinning an out-of-range point to the edge would
    /// draw a value the data does not contain.
    pub fn position(&self, value: f64, start: f32, end: f32) -> f32 {
        let span = self.span();
        if span <= 0.0 || !value.is_finite() {
            return start;
        }
        let t = (value - self.min) / span;
        start + (end - start) * t as f32
    }

    /// Where zero sits on `[start, end]` — the baseline every bar grows from. Falls
    /// back to the nearer end when the domain does not contain zero, so a bar chart
    /// of values that never reach zero still has somewhere to grow from.
    pub fn baseline(&self, start: f32, end: f32) -> f32 {
        let zero = self.min.clamp(0.0_f64.min(self.max), self.max.max(0.0));
        self.position(zero.clamp(self.min, self.max), start, end)
    }
}

/// The mantissas a step is allowed to have, and the breakpoints between them.
///
/// Each breakpoint is the **geometric** midpoint of its neighbours — √2, √5,
/// √12.5, √50 — so a raw step is rounded to whichever candidate is proportionally
/// closest, which is how the eye compares intervals. Arithmetic midpoints would
/// bias every choice towards the larger candidate.
const STEP_MANTISSAS: [(f64, f64); 5] = [
    (std::f64::consts::SQRT_2, 1.0),  // ..1.414 → 1
    (2.236_067_977_499_79, 2.0),      // ..√5    → 2
    (3.535_533_905_932_737_6, 2.5),   // ..√12.5 → 2.5
    (7.071_067_811_865_475, 5.0),     // ..√50   → 5
    (f64::INFINITY, 10.0),            // above   → the next decade
];

/// Rounds a raw interval to the nearest nice step — see [`STEP_MANTISSAS`].
fn nice_step(raw: f64) -> f64 {
    if !raw.is_finite() || raw <= 0.0 {
        return 1.0;
    }
    let exponent = raw.log10().floor();
    let power = 10_f64.powf(exponent);
    let fraction = raw / power;
    let nice = STEP_MANTISSAS
        .iter()
        .find(|(breakpoint, _)| fraction < *breakpoint)
        .map(|(_, mantissa)| *mantissa)
        .unwrap_or(10.0);
    let step = nice * power;
    // Underflow at the bottom of the exponent range, or a power that overflowed:
    // fall back to something a division can survive.
    if step > 0.0 && step.is_finite() { step } else { 1.0 }
}

/// Clamps a pair of bounds into something arithmetic can be done on: finite, in
/// order, and narrow enough that `max - min` does not overflow to infinity.
fn sanitise_domain(min: f64, max: f64) -> (f64, f64) {
    let (min, max) = match (min.is_finite(), max.is_finite()) {
        (true, true) => (min, max),
        (true, false) => (min, min),
        (false, true) => (max, max),
        (false, false) => (0.0, 1.0),
    };
    let (min, max) = if min <= max { (min, max) } else { (max, min) };
    // A quarter of the range each way still leaves a span that subtracts finitely.
    const LIMIT: f64 = f64::MAX / 4.0;
    (min.clamp(-LIMIT, LIMIT), max.clamp(-LIMIT, LIMIT))
}

/// Relative tolerance for the divisions that decide a tick's index. Chosen well
/// above `f64::EPSILON` (~2.2e-16) so it absorbs the error of a division and a
/// multiplication, and far below any step a label could distinguish.
const QUOTIENT_TOLERANCE: f64 = 1e-9;

/// The tolerance is relative — the error in `min / step` grows with the quotient —
/// but it is capped, and the cap is load-bearing.
///
/// Uncapped, a quotient of 5e15 gets a tolerance of 5e6, which swallows the whole
/// comparison: *every* value counts as "a hair below the next integer", `floor`
/// starts behaving like `ceil`, and axes at large magnitudes collapse to the
/// fallback. At a thousandth of a step the snap stays invisible and can never cost
/// the data its coverage.
const MAX_QUOTIENT_TOLERANCE: f64 = 1e-3;

fn quotient_tolerance(value: f64) -> f64 {
    (QUOTIENT_TOLERANCE * value.abs().max(1.0)).min(MAX_QUOTIENT_TOLERANCE)
}

/// `floor`, but a quotient that is a hair under an integer counts as that integer.
///
/// `0.30000000000000004 / 0.1` is `3.0000000000000004`; plain `ceil` makes that `4`
/// and hangs an empty tick above the data.
pub fn floor_with_tolerance(value: f64) -> f64 {
    let floored = value.floor();
    if value - floored > 1.0 - quotient_tolerance(value) {
        floored + 1.0
    } else {
        floored
    }
}

/// `ceil`, with the mirror-image tolerance.
pub fn ceil_with_tolerance(value: f64) -> f64 {
    let ceiled = value.ceil();
    if ceiled - value > 1.0 - quotient_tolerance(value) {
        ceiled - 1.0
    } else {
        ceiled
    }
}

/// Snaps a value to `decimals` places, so that a tick's stored value is exactly the
/// number its label prints — `-0.30000000000000004` becomes `-0.3`.
///
/// The guard is the point: snapping is allowed to repair *representation* error and
/// nothing else. A value smaller than the snapping grid — a tick at 1e-300 against a
/// 12-place grid — would otherwise round to zero, silently collapsing an entire axis
/// onto one value. If the snapped result is not within a whisker of the original,
/// the original wins.
fn snap_to_decimals(value: f64, decimals: u8) -> f64 {
    let scale = 10_f64.powi(decimals.min(MAX_DECIMALS) as i32);
    let scaled = value * scale;
    if !scaled.is_finite() {
        return value;
    }
    let snapped = scaled.round() / scale;
    if (snapped - value).abs() <= value.abs() * 1e-9 { snapped } else { value }
}

/// A categorical axis: `count` equal slots across a pixel interval, with padding
/// between them and at the ends.
///
/// Padding is expressed as a fraction of the slot pitch rather than in pixels, which
/// is what keeps a chart looking deliberate at every size: four bars in a 200px plot
/// and four in a 2000px plot have the same *proportion* of air between them.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct BandScale {
    count: usize,
    start: f32,
    end: f32,
    inner_padding: f32,
    outer_padding: f32,
}

impl BandScale {
    /// Air between neighbouring bands, as a fraction of the pitch. 0.2 keeps bars
    /// clearly separate without the chart reading as a row of stripes.
    pub const DEFAULT_INNER_PADDING: f32 = 0.2;
    /// Air before the first band and after the last, as a fraction of the pitch.
    /// Half the inner padding, so the ends look like ends rather than like a missing
    /// neighbour.
    pub const DEFAULT_OUTER_PADDING: f32 = 0.1;

    pub fn new(count: usize, start: f32, end: f32) -> Self {
        Self {
            count,
            start,
            end,
            inner_padding: Self::DEFAULT_INNER_PADDING,
            outer_padding: Self::DEFAULT_OUTER_PADDING,
        }
    }

    pub fn with_padding(self, inner: f32, outer: f32) -> Self {
        Self {
            inner_padding: inner.clamp(0.0, 0.95),
            outer_padding: outer.clamp(0.0, 5.0),
            ..self
        }
    }

    pub fn count(&self) -> usize {
        self.count
    }

    /// Distance from one band's start to the next's.
    pub fn pitch(&self) -> f32 {
        if self.count == 0 {
            return 0.0;
        }
        let divisor = self.count as f32 - self.inner_padding + 2.0 * self.outer_padding;
        if divisor <= 0.0 {
            return 0.0;
        }
        (self.end - self.start) / divisor
    }

    /// The drawable width of one band.
    pub fn band_width(&self) -> f32 {
        (self.pitch() * (1.0 - self.inner_padding)).max(0.0)
    }

    /// The start of band `index`, in pixels. Out-of-range indices extrapolate rather
    /// than clamp, which keeps the arithmetic honest if a caller ever iterates past
    /// the end — the band is simply outside the plot, and the plot's own bounds are
    /// what clips it.
    pub fn band_start(&self, index: usize) -> f32 {
        let pitch = self.pitch();
        self.start + pitch * self.outer_padding + pitch * index as f32
    }

    /// The centre of band `index`: where a tick, a line vertex or a dot goes.
    pub fn centre(&self, index: usize) -> f32 {
        self.band_start(index) + self.band_width() * 0.5
    }

    /// Splits a band into `count` sub-bands separated by `gap` pixels — grouped bars.
    ///
    /// The gap is in pixels, not a fraction, because it is the 2px surface gap that
    /// separates touching marks and it must not scale with the chart. When there is
    /// not enough room for the gaps at all, they are given up before the bars are:
    /// a 1px sliver of a bar still carries its value, while a bar reduced to nothing
    /// by its own separator does not.
    pub fn sub_band(&self, index: usize, sub: usize, count: usize, gap: f32) -> (f32, f32) {
        let width = self.band_width();
        if count <= 1 {
            return (self.band_start(index), width);
        }
        let gaps = gap * (count - 1) as f32;
        let gap = if gaps < width { gap } else { 0.0 };
        let sub_width = ((width - gap * (count - 1) as f32) / count as f32).max(0.0);
        let start = self.band_start(index) + (sub_width + gap) * sub as f32;
        (start, sub_width)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ticks(min: f64, max: f64) -> Vec<f64> {
        ValueScale::nice(min, max, TickOptions::FROM_ZERO).ticks().to_vec()
    }

    /// The example in the module docs, and the reason this crate has a scale module.
    #[test]
    fn an_awkward_range_reads_as_round_numbers() {
        assert_eq!(ticks(0.0, 94.8), vec![0.0, 25.0, 50.0, 75.0, 100.0]);
        assert_eq!(ticks(0.0, 23.7), vec![0.0, 5.0, 10.0, 15.0, 20.0, 25.0]);
        assert_eq!(ticks(3.0, 17.0), vec![0.0, 5.0, 10.0, 15.0, 20.0]);
    }

    #[test]
    fn every_step_is_a_nice_mantissa_times_a_power_of_ten() {
        let mut checked = 0;
        for max in [1.0_f64, 3.3, 7.0, 12.0, 48.0, 99.0, 101.0, 1234.0, 98765.0, 1e9] {
            for target in 2..=12 {
                let scale = ValueScale::nice(0.0, max, TickOptions::FROM_ZERO.with_target(target));
                let mantissa = scale.step() / 10_f64.powf(scale.step().log10().floor());
                assert!(
                    [1.0, 2.0, 2.5, 5.0].iter().any(|m| (mantissa - m).abs() < 1e-9),
                    "step {} has mantissa {mantissa}",
                    scale.step()
                );
                checked += 1;
            }
        }
        assert_eq!(checked, 10 * 11);
    }

    #[test]
    fn the_domain_always_covers_the_data() {
        for (min, max) in [
            (0.0, 1.0),
            (3.0, 17.0),
            (-4.5, 9.25),
            (-100.0, -3.0),
            (0.001, 0.009),
            (1e6, 3e6),
        ] {
            for include_zero in [true, false] {
                let scale = ValueScale::nice(min, max, TickOptions { target: 5, include_zero });
                assert!(scale.min() <= min, "{min}..{max}: domain starts at {}", scale.min());
                assert!(scale.max() >= max, "{min}..{max}: domain ends at {}", scale.max());
                assert_eq!(scale.ticks().first().copied(), Some(scale.min()));
                assert!((scale.ticks().last().copied().unwrap() - scale.max()).abs() < scale.step() * 1e-6);
            }
        }
    }

    #[test]
    fn ticks_are_sorted_finite_and_evenly_spaced() {
        for (min, max) in [(0.0, 1.0), (-7.0, 7.0), (-1e-3, 5e-3), (2e11, 9e11)] {
            let scale = ValueScale::nice(min, max, TickOptions::FREE);
            let ticks = scale.ticks();
            assert!(ticks.len() >= 2);
            for pair in ticks.windows(2) {
                assert!(pair[0].is_finite() && pair[1].is_finite());
                assert!(pair[1] > pair[0], "{ticks:?}");
                let gap = pair[1] - pair[0];
                assert!(
                    (gap - scale.step()).abs() <= scale.step() * 1e-9,
                    "uneven gap {gap} against step {}",
                    scale.step()
                );
            }
        }
    }

    /// A range that straddles zero must put a tick *on* zero — the reader measures
    /// negative against positive there, and an axis that skips it is unreadable.
    #[test]
    fn a_range_spanning_zero_has_a_tick_at_zero() {
        for (min, max) in [(-3.0, 7.0), (-0.4, 0.9), (-1e5, 2e5), (-7.5, 0.5)] {
            let scale = ValueScale::nice(min, max, TickOptions::FREE);
            assert!(
                scale.ticks().contains(&0.0),
                "{min}..{max} produced {:?}",
                scale.ticks()
            );
            assert!(scale.contains_zero());
        }
    }

    #[test]
    fn an_all_negative_range_stays_negative_and_reads_forwards() {
        let scale = ValueScale::nice(-50.0, -12.0, TickOptions::FREE);
        assert_eq!(scale.ticks(), &[-50.0, -40.0, -30.0, -20.0, -10.0]);
        // With a baseline required, zero joins the axis at the top.
        let from_zero = ValueScale::nice(-50.0, -12.0, TickOptions::FROM_ZERO);
        assert_eq!(from_zero.max(), 0.0);
        assert!(from_zero.contains_zero());
    }

    /// The float trap this module's tolerance exists for, in the form it actually
    /// arrives in: a stacked total of `0.1 + 0.2` is `0.30000000000000004`, whose
    /// quotient by a step of `0.1` is `3.0000000000000004`. A plain `ceil` reads
    /// that as four steps and hangs an empty tick at `0.4` above data that stops at
    /// `0.3` — a visible, permanent gap at the top of the axis.
    #[test]
    fn a_float_sum_does_not_grow_a_phantom_tick() {
        let total = 0.1 + 0.2;
        assert_ne!(total, 0.3, "the premise: the sum is not exactly 0.3");

        let scale = ValueScale::nice(0.0, total, TickOptions::FROM_ZERO.with_target(4));
        assert_eq!(scale.step(), 0.1);
        assert_eq!(scale.ticks(), &[0.0, 0.1, 0.2, 0.3]);
        // And the domain's own ends are the snapped values, not 0.30000000000000004.
        assert_eq!(scale.max(), 0.3);
    }

    #[test]
    fn identical_values_get_an_axis_around_them_not_a_zero_span() {
        // With a baseline, the value sits at the top of a 0-based axis.
        let bar = ValueScale::nice(5.0, 5.0, TickOptions::FROM_ZERO);
        assert_eq!(bar.min(), 0.0);
        assert!(bar.max() >= 5.0);
        assert!(bar.span() > 0.0);

        // Without one, it sits in the middle of a domain of its own magnitude.
        let line = ValueScale::nice(5.0, 5.0, TickOptions::FREE);
        assert!(line.min() < 5.0 && line.max() > 5.0, "{:?}", line.ticks());
        assert!(line.ticks().len() >= 2);

        // Identical *negative* values, the case that catches a missing `abs`.
        let negative = ValueScale::nice(-8.0, -8.0, TickOptions::FREE);
        assert!(negative.min() < -8.0 && negative.max() > -8.0, "{:?}", negative.ticks());
    }

    #[test]
    fn all_zero_data_gets_the_unit_interval_rather_than_a_point() {
        let scale = ValueScale::nice(0.0, 0.0, TickOptions::FROM_ZERO);
        assert_eq!(scale.min(), 0.0);
        assert_eq!(scale.max(), 1.0);
        assert!(scale.ticks().len() >= 2);
        assert!(scale.ticks().iter().all(|t| t.is_finite()));
    }

    #[test]
    fn a_reversed_pair_is_read_in_the_order_it_makes_sense_in() {
        assert_eq!(
            ValueScale::nice(90.0, 10.0, TickOptions::FREE).ticks(),
            ValueScale::nice(10.0, 90.0, TickOptions::FREE).ticks()
        );
    }

    #[test]
    fn non_finite_bounds_resolve_to_an_axis_instead_of_a_nan() {
        for (min, max) in [
            (f64::NAN, f64::NAN),
            (f64::NAN, 10.0),
            (0.0, f64::NAN),
            (f64::NEG_INFINITY, f64::INFINITY),
            (f64::NEG_INFINITY, 0.0),
            (0.0, f64::INFINITY),
        ] {
            let scale = ValueScale::nice(min, max, TickOptions::FREE);
            assert!(scale.span() > 0.0, "{min}..{max} produced a degenerate span");
            assert!(scale.step() > 0.0 && scale.step().is_finite());
            assert!(!scale.ticks().is_empty());
            assert!(scale.ticks().iter().all(|t| t.is_finite()), "{:?}", scale.ticks());
            assert!(scale.ticks().len() <= MAX_TICKS);
        }
    }

    /// Both ends of what an `f64` carries. The requirement is not a *good* axis —
    /// there isn't one — but a finite, ordered, bounded one.
    #[test]
    fn extreme_magnitudes_stay_finite_and_bounded() {
        for (min, max) in [
            (0.0, 1e300),
            (-1e300, 1e300),
            (1e-300, 2e-300),
            (0.0, f64::MIN_POSITIVE),
            (-f64::MAX, f64::MAX),
            (f64::MAX, f64::MAX),
            (1e17, 1e17 + 1.0),
        ] {
            let scale = ValueScale::nice(min, max, TickOptions::FREE);
            assert!(scale.step() > 0.0 && scale.step().is_finite(), "{min}..{max}");
            assert!(scale.span() > 0.0 && scale.span().is_finite(), "{min}..{max}");
            assert!(!scale.ticks().is_empty() && scale.ticks().len() <= MAX_TICKS, "{min}..{max}");
            assert!(scale.ticks().iter().all(|t| t.is_finite()), "{min}..{max}");
            assert!(scale.ticks().windows(2).all(|w| w[1] > w[0]), "{min}..{max}");
        }
    }

    #[test]
    fn the_tick_count_lands_near_the_target() {
        for target in 3..=10_usize {
            for max in [1.0_f64, 7.0, 42.0, 137.0, 999.0, 1e5] {
                let count = ValueScale::nice(0.0, max, TickOptions::FROM_ZERO.with_target(target))
                    .ticks()
                    .len();
                assert!(
                    count >= 2 && count <= target * 2 + 1,
                    "target {target} over 0..{max} gave {count} ticks"
                );
            }
        }
    }

    #[test]
    fn a_nonsense_target_is_clamped_rather_than_obeyed() {
        for target in [0_usize, 1, 1000, usize::MAX] {
            let scale = ValueScale::nice(0.0, 100.0, TickOptions { target, include_zero: true });
            assert!(scale.ticks().len() >= 2);
            assert!(scale.ticks().len() <= MAX_TICKS);
        }
    }

    #[test]
    fn decimals_match_the_step_so_labels_share_a_shape() {
        // 0, 0.25, 0.5, 0.75, 1 — two places, on every label including "0.00".
        let unit = ValueScale::nice(0.0, 1.0, TickOptions::FROM_ZERO);
        assert_eq!(unit.step(), 0.25);
        assert_eq!(unit.decimals(), 2);
        assert_eq!(ValueScale::nice(0.0, 100.0, TickOptions::FROM_ZERO).decimals(), 0);
        assert_eq!(ValueScale::nice(0.0, 0.5, TickOptions::FROM_ZERO).decimals(), 1);
    }

    #[test]
    fn position_maps_the_domain_onto_the_pixel_interval_in_either_direction() {
        let scale = ValueScale::nice(0.0, 100.0, TickOptions::FROM_ZERO);
        // A y axis: top of the plot is the smaller pixel value.
        assert!((scale.position(0.0, 200.0, 0.0) - 200.0).abs() < 1e-4);
        assert!((scale.position(100.0, 200.0, 0.0) - 0.0).abs() < 1e-4);
        assert!((scale.position(50.0, 200.0, 0.0) - 100.0).abs() < 1e-4);
        // An x axis.
        assert!((scale.position(25.0, 0.0, 400.0) - 100.0).abs() < 1e-4);
        // Out of domain extrapolates rather than clamping.
        assert!(scale.position(200.0, 0.0, 400.0) > 400.0);
        // A non-finite value cannot produce a NaN vertex.
        assert_eq!(scale.position(f64::NAN, 0.0, 400.0), 0.0);
    }

    #[test]
    fn the_baseline_is_zero_where_zero_exists_and_the_near_end_otherwise() {
        let spanning = ValueScale::nice(-50.0, 50.0, TickOptions::FREE);
        assert!((spanning.baseline(200.0, 0.0) - 100.0).abs() < 1e-4);
        // An axis floating above zero puts the baseline at its own floor.
        let floating = ValueScale::nice(80.0, 120.0, TickOptions::FREE);
        assert!((floating.baseline(200.0, 0.0) - 200.0).abs() < 1e-4);
        // And one entirely below zero puts it at the top.
        let below = ValueScale::nice(-120.0, -80.0, TickOptions::FREE);
        assert!((below.baseline(200.0, 0.0) - 0.0).abs() < 1e-4);
    }

    #[test]
    fn nice_step_rounds_at_the_geometric_midpoints() {
        assert_eq!(nice_step(1.0), 1.0);
        assert_eq!(nice_step(1.4), 1.0);
        assert_eq!(nice_step(1.5), 2.0);
        assert_eq!(nice_step(2.2), 2.0);
        assert_eq!(nice_step(2.3), 2.5);
        assert_eq!(nice_step(3.5), 2.5);
        assert_eq!(nice_step(3.6), 5.0);
        assert_eq!(nice_step(6.9), 5.0);
        assert_eq!(nice_step(7.1), 10.0);
        assert_eq!(nice_step(0.023), 0.025);
        assert_eq!(nice_step(23.7), 25.0);
        // Degenerate input never returns zero, which would divide by nothing later.
        assert_eq!(nice_step(0.0), 1.0);
        assert_eq!(nice_step(-5.0), 1.0);
        assert_eq!(nice_step(f64::NAN), 1.0);
        assert!(nice_step(f64::MIN_POSITIVE) > 0.0);
    }

    #[test]
    fn the_quotient_tolerance_absorbs_division_error_without_moving_real_values() {
        assert_eq!(floor_with_tolerance(-3.000_000_000_000_000_4), -3.0);
        assert_eq!(floor_with_tolerance(-3.5), -4.0);
        assert_eq!(floor_with_tolerance(3.999_999_999_999_999), 4.0);
        assert_eq!(ceil_with_tolerance(3.000_000_000_000_000_4), 3.0);
        assert_eq!(ceil_with_tolerance(3.5), 4.0);
        assert_eq!(ceil_with_tolerance(-2.999_999_999_999_999_6), -3.0);
    }

    /// The tolerance is relative, and the cap on it is what keeps `floor` behaving
    /// like `floor` at magnitudes where a relative tolerance would exceed 1 and
    /// swallow the comparison whole.
    #[test]
    fn a_huge_quotient_still_floors_and_ceils_normally() {
        assert_eq!(floor_with_tolerance(5e15), 5e15);
        assert_eq!(ceil_with_tolerance(5e15 + 1.0), 5e15 + 1.0);
        assert_eq!(floor_with_tolerance(-5e15), -5e15);
        // And the axis that exposed it: two ulps apart, at 1e16. Before the cap this
        // fell all the way through to the fallback and reported an axis of 0..1.
        let scale = ValueScale::nice(1e16, 1e16 + 2.0, TickOptions { target: 2, include_zero: false });
        assert_eq!(scale.min(), 1e16);
        assert!(scale.max() >= 1e16 + 2.0, "{}", scale.max());
    }

    /// Near the top of `f64`'s precision the requested step can be finer than the
    /// gap between representable numbers, and the axis would repeat a value.
    #[test]
    fn a_step_finer_than_the_float_grid_is_widened_until_ticks_differ() {
        for target in 2..=8 {
            let scale = ValueScale::nice(
                1e16,
                1e16 + 2.0,
                TickOptions { target, include_zero: false },
            );
            let ticks = scale.ticks();
            assert!(
                ticks.windows(2).all(|pair| pair[1] > pair[0]),
                "target {target} repeated a value: {ticks:?}"
            );
            assert!(scale.step() >= 2.0, "target {target}: step {}", scale.step());
        }
    }

    #[test]
    fn bands_divide_the_axis_with_padding_at_the_ends() {
        let scale = BandScale::new(4, 0.0, 400.0);
        assert!(scale.band_start(0) > 0.0, "outer padding leaves air before the first band");
        assert!(scale.band_start(3) + scale.band_width() < 400.0);
        // Bands are evenly pitched, in order, and do not overlap.
        for index in 0..3 {
            let gap = scale.band_start(index + 1) - (scale.band_start(index) + scale.band_width());
            assert!(gap > 0.0, "bands {index} and {} touch", index + 1);
        }
        assert!((scale.centre(0) - (scale.band_start(0) + scale.band_width() / 2.0)).abs() < 1e-4);
    }

    #[test]
    fn a_band_scale_with_no_categories_has_no_width_rather_than_a_nan() {
        let empty = BandScale::new(0, 0.0, 400.0);
        assert_eq!(empty.pitch(), 0.0);
        assert_eq!(empty.band_width(), 0.0);
        assert!(empty.band_start(0).is_finite());
    }

    #[test]
    fn grouped_sub_bands_share_the_band_and_give_up_gaps_before_width() {
        let scale = BandScale::new(3, 0.0, 300.0);
        let width = scale.band_width();
        let (first, sub_width) = scale.sub_band(0, 0, 3, 2.0);
        let (last, last_width) = scale.sub_band(0, 2, 3, 2.0);
        assert!((first - scale.band_start(0)).abs() < 1e-4);
        assert!((last + last_width - (scale.band_start(0) + width)).abs() < 1e-3);
        assert!((sub_width * 3.0 + 4.0 - width).abs() < 1e-3);

        // One series: the whole band, no gap arithmetic at all.
        assert_eq!(scale.sub_band(0, 0, 1, 2.0), (scale.band_start(0), width));

        // A band too narrow for its gaps keeps the bars and drops the separators.
        let tight = BandScale::new(20, 0.0, 100.0);
        let (_, narrow) = tight.sub_band(0, 0, 4, 2.0);
        assert!(narrow > 0.0, "a sliver of bar beats no bar");
    }
}
