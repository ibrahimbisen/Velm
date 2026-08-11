//! The tick algorithm, swept rather than spot-checked.
//!
//! `scale.rs`'s own tests assert specific axes for specific ranges — that `0..94.8`
//! reads `0, 25, 50, 75, 100`. This file asserts the *invariants* across every kind
//! of input a spreadsheet column can be, including several thousand generated ones.
//! The two are complementary: the unit tests would still pass if the algorithm
//! silently stopped covering the data at some magnitude, and this file would still
//! pass if it produced technically-valid but ugly axes.
//!
//! The five invariants, which hold for **every** input with no exceptions:
//!
//! 1. There are at least two ticks, and never more than the cap.
//! 2. Every tick is finite, and they strictly increase.
//! 3. The spacing is uniform, and equals the reported step.
//! 4. The domain covers the data, and its ends are the first and last tick.
//! 5. Every tick is a whole multiple of the step — which is what makes an axis read
//!    as a scale rather than as five arbitrary numbers.

use vellum_chart::{TickOptions, ValueScale};

/// The cap in `scale.rs`. Duplicated deliberately: if that constant is raised, this
/// file should be a deliberate part of the change rather than silently following.
const MAX_TICKS: usize = 101;

fn check(min: f64, max: f64, options: TickOptions) -> ValueScale {
    let scale = ValueScale::nice(min, max, options);
    let ticks = scale.ticks();
    let context = format!("{min}..{max} (target {}, zero {})", options.target, options.include_zero);

    // 1. A useful number of ticks.
    assert!(ticks.len() >= 2, "{context}: only {} ticks", ticks.len());
    assert!(ticks.len() <= MAX_TICKS, "{context}: {} ticks", ticks.len());

    // 2. Finite and strictly increasing.
    assert!(scale.step().is_finite() && scale.step() > 0.0, "{context}: step {}", scale.step());
    assert!(scale.min().is_finite() && scale.max().is_finite(), "{context}");
    for pair in ticks.windows(2) {
        assert!(pair[0].is_finite() && pair[1].is_finite(), "{context}: {ticks:?}");
        assert!(pair[1] > pair[0], "{context}: not increasing: {ticks:?}");
    }

    // 3. Uniform spacing.
    for pair in ticks.windows(2) {
        let gap = pair[1] - pair[0];
        assert!(
            (gap - scale.step()).abs() <= scale.step().abs() * 1e-6,
            "{context}: gap {gap} against step {}",
            scale.step()
        );
    }

    // 4. The domain covers the data and is bounded by its own ticks.
    assert_eq!(ticks.first().copied(), Some(scale.min()), "{context}");
    assert_eq!(ticks.last().copied(), Some(scale.max()), "{context}");
    if min.is_finite() && max.is_finite() {
        let (lo, hi) = if min <= max { (min, max) } else { (max, min) };
        // Coverage is only meaningful where the input was not clamped. A bound at
        // `f64::MAX` is deliberately reined in — an axis there cannot be widened
        // outwards to a round number without overflowing — so those inputs are
        // held to the other four invariants and not to this one.
        let clamped = lo.abs() >= f64::MAX / 8.0 || hi.abs() >= f64::MAX / 8.0;
        if !clamped && hi - lo < f64::MAX / 8.0 {
            assert!(scale.min() <= lo || (scale.min() - lo).abs() <= scale.step() * 1e-6, "{context}");
            assert!(scale.max() >= hi || (scale.max() - hi).abs() <= scale.step() * 1e-6, "{context}");
        }
    }

    // 5. Every tick is a whole multiple of the step.
    for tick in ticks {
        let multiple = tick / scale.step();
        assert!(
            (multiple - multiple.round()).abs() < 1e-6,
            "{context}: {tick} is not a multiple of {}",
            scale.step()
        );
    }

    scale
}

fn both_options(min: f64, max: f64) {
    for target in [2_usize, 3, 5, 8, 12] {
        for include_zero in [true, false] {
            check(min, max, TickOptions { target, include_zero });
        }
    }
}

#[test]
fn the_ordinary_cases() {
    for (min, max) in [
        (0.0, 1.0),
        (0.0, 10.0),
        (0.0, 94.8),
        (3.0, 17.0),
        (12.0, 41.0),
        (0.0, 1e6),
        (0.001, 0.009),
        (1.5, 1.9),
    ] {
        both_options(min, max);
    }
}

/// Every value identical — a column of the same number, which is what a filtered
/// spreadsheet produces constantly.
#[test]
fn all_equal_values() {
    for value in [0.0, 1.0, -1.0, 5.0, -8.0, 1e9, -1e9, 1e-9, f64::MIN_POSITIVE, -f64::MAX / 8.0] {
        both_options(value, value);
        // The value is inside the axis it produced, wherever it was.
        for include_zero in [true, false] {
            let scale = check(value, value, TickOptions { target: 5, include_zero });
            assert!(
                scale.min() <= value && value <= scale.max(),
                "{value} fell outside its own axis {}..{}",
                scale.min(),
                scale.max()
            );
        }
    }
}

/// One row of data. Identical arithmetic to the all-equal case, and worth its own
/// name because it is the one a caller hits first.
#[test]
fn a_single_data_point() {
    for value in [0.0, 7.5, -7.5, 1234.0] {
        let scale = check(value, value, TickOptions::FREE);
        assert!(scale.span() > 0.0);
    }
}

#[test]
fn an_empty_series_falls_back_to_a_readable_axis() {
    // What `ChartGeometry` uses when a dataset has no values at all.
    let scale = ValueScale::fallback();
    assert!(scale.ticks().len() >= 2);
    assert_eq!(scale.min(), 0.0);
    assert_eq!(scale.max(), 1.0);
    assert!(scale.ticks().iter().all(|tick| tick.is_finite()));
}

#[test]
fn entirely_negative_ranges() {
    for (min, max) in [(-1.0, -0.1), (-50.0, -12.0), (-1e6, -1.0), (-0.009, -0.001)] {
        both_options(min, max);
        let free = check(min, max, TickOptions::FREE);
        assert!(free.max() <= 0.0, "a negative range must not sprout a positive end");
        // Asking for a baseline pulls zero in, and only then.
        let zeroed = check(min, max, TickOptions::FROM_ZERO);
        assert_eq!(zeroed.max(), 0.0);
    }
}

#[test]
fn ranges_spanning_zero_put_a_tick_on_zero() {
    for (min, max) in [
        (-1.0, 1.0),
        (-3.0, 7.0),
        (-0.4, 0.9),
        (-1e5, 2e5),
        (-7.5, 0.5),
        (-1e-6, 1e-6),
        (-1e12, 3e11),
    ] {
        for target in [2_usize, 4, 5, 7, 11] {
            let scale = check(min, max, TickOptions { target, include_zero: false });
            assert!(
                scale.ticks().contains(&0.0),
                "{min}..{max} target {target}: no zero in {:?}",
                scale.ticks()
            );
        }
    }
}

#[test]
fn very_large_and_very_small_magnitudes() {
    for (min, max) in [
        (0.0, 1e15),
        (0.0, 1e300),
        (-1e300, 1e300),
        (1e-300, 2e-300),
        (0.0, f64::MIN_POSITIVE),
        (f64::MIN_POSITIVE, f64::MIN_POSITIVE * 4.0),
        (-f64::MAX, f64::MAX),
        (f64::MAX, f64::MAX),
        (-f64::MAX, -f64::MAX),
        (1e17, 1e17 + 1.0),
        (1e16, 1e16 + 2.0),
    ] {
        both_options(min, max);
    }
}

#[test]
fn non_finite_bounds_never_reach_the_output() {
    for (min, max) in [
        (f64::NAN, f64::NAN),
        (f64::NAN, 10.0),
        (0.0, f64::NAN),
        (f64::INFINITY, f64::INFINITY),
        (f64::NEG_INFINITY, f64::INFINITY),
        (f64::NEG_INFINITY, 0.0),
        (0.0, f64::INFINITY),
        (f64::INFINITY, f64::NEG_INFINITY),
    ] {
        both_options(min, max);
    }
}

#[test]
fn a_reversed_range_is_read_the_way_round_it_makes_sense() {
    for (min, max) in [(10.0, 1.0), (0.0, -5.0), (1e9, -1e9)] {
        let reversed = check(min, max, TickOptions::FREE);
        let forwards = check(max, min, TickOptions::FREE);
        assert_eq!(reversed.ticks(), forwards.ticks());
    }
}

/// A deterministic sweep. Not property-based testing with a shrinker, but the same
/// intent: thousands of ranges no one thought to write down, at magnitudes from
/// 1e-12 to 1e12, with a fixed seed so a failure is reproducible from the message
/// alone.
#[test]
fn a_generated_sweep_of_several_thousand_ranges() {
    // xorshift64*, inlined: this file will not take a dependency to make noise.
    let mut state = 0x2545_F491_4F6C_DD1D_u64;
    let mut next = move || {
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        state = state.wrapping_mul(0x2545_F491_4F6C_DD1D);
        (state >> 11) as f64 / (1_u64 << 53) as f64
    };

    let mut checked = 0;
    for _ in 0..2000 {
        let exponent = next() * 24.0 - 12.0;
        let magnitude = 10_f64.powf(exponent);
        let a = (next() - 0.5) * 2.0 * magnitude;
        let b = (next() - 0.5) * 2.0 * magnitude;
        let target = 2 + (next() * 10.0) as usize;
        check(a.min(b), a.max(b), TickOptions { target, include_zero: next() < 0.5 });
        checked += 1;
    }
    assert_eq!(checked, 2000);
}

/// The tick *count* has to land near what was asked for, or the "target" is a lie
/// and axes come out either bare or crowded.
#[test]
fn the_count_tracks_the_target() {
    for target in 3..=10_usize {
        for max in [1.0_f64, 7.0, 42.0, 137.0, 999.0, 1e5, 1e-5] {
            let count = ValueScale::nice(0.0, max, TickOptions { target, include_zero: true })
                .ticks()
                .len();
            assert!(
                count >= target.min(3) - 1 && count <= target * 2 + 1,
                "target {target} over 0..{max} gave {count}"
            );
        }
    }
}

/// Two ranges that differ by a hair must not produce wildly different axes: a chart
/// that redraws as data streams in should not jump between step sizes on noise.
#[test]
fn a_hair_of_extra_data_does_not_restructure_the_axis() {
    let base = ValueScale::nice(0.0, 100.0, TickOptions::FROM_ZERO);
    for delta in [1e-9, 1e-6, 0.001] {
        let nudged = ValueScale::nice(0.0, 100.0 + delta, TickOptions::FROM_ZERO);
        assert_eq!(base.step(), nudged.step(), "a nudge of {delta} changed the step");
    }
}
