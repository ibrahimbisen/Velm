//! Turning a number into the string a reader sees.
//!
//! Axis labels are where a chart's arithmetic becomes typography, and it is easy to
//! undo good tick values with bad formatting: `1000000` is unreadable, `1.0e6` is
//! engineering notation in a business chart, and `0.30000000000000004` is a float
//! leaking through. The rules here are narrow and testable.
//!
//! - **Group thousands.** `12,000`, not `12000`.
//! - **One decimal count per axis.** Ticks are `0.0, 0.5, 1.0`, never `0, 0.5, 1` —
//!   the decimal count comes from the tick *step*, so every label on an axis has the
//!   same shape and the digits line up under tabular figures.
//! - **Compact only when the axis needs it.** Past five digits the grouped form
//!   costs more width than the axis has, so `12.9K` and `4.2M` take over. Below
//!   that, the exact number is always better.
//! - **Never `-0`.** A tick at negative zero is a rounding artefact, not a value.
//!
//! Separators are fixed to `,` and `.`. Locale-aware formatting needs the platform's
//! locale database, which this crate deliberately does not have; when Vellum grows
//! a locale layer this is the one place it plugs in.

use serde::{Deserialize, Serialize};

/// How a value is spelled out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum NumberStyle {
    /// `-1,204.50` — the exact value, grouped.
    #[default]
    Plain,
    /// `12.9K`, `4.2M`, `1.3B`, `2.7T` — for axes whose values run past five digits.
    Compact,
    /// `1.2e-9` — for magnitudes where neither of the above says anything useful.
    Scientific,
    /// A fraction rendered as a percentage: `0.125` becomes `12.5%`.
    Percent,
}

impl NumberStyle {
    /// Picks a style for an axis from its step and its largest magnitude.
    ///
    /// The thresholds are about *label width*, not about taste. Under 1e5 a grouped
    /// label is at most seven characters and fits any axis. Past 1e15 an `f64` has
    /// no digits left to spare below the decimal point, and under 1e-4 a plain
    /// rendering is mostly leading zeros — both are cases where scientific notation
    /// is the honest form rather than a fallback.
    pub fn for_axis(step: f64, max_magnitude: f64) -> Self {
        let step = step.abs();
        if !step.is_finite() || !max_magnitude.is_finite() {
            return Self::Plain;
        }
        if max_magnitude >= 1e15 || (step > 0.0 && step < 1e-4) {
            Self::Scientific
        } else if max_magnitude >= 1e5 {
            Self::Compact
        } else {
            Self::Plain
        }
    }
}

/// How many decimal places a step needs to be written exactly.
///
/// Driven by the step rather than by the values, so an axis of `0, 2.5, 5` labels
/// every tick with one decimal instead of mixing `0` with `2.5`. Capped at
/// [`MAX_DECIMALS`]: past that an `f64` is not carrying the digits anyway, and the
/// label would be printing rounding noise.
pub fn decimals_for_step(step: f64) -> u8 {
    let step = step.abs();
    if !step.is_finite() || step == 0.0 {
        return 0;
    }
    for decimals in 0..=MAX_DECIMALS {
        let scale = 10_f64.powi(decimals as i32);
        let scaled = step * scale;
        // A step is "written exactly" at this many places when scaling it by the
        // matching power of ten lands on an integer. The tolerance is relative, so
        // it holds for 0.001 and for 2.5e11 alike.
        if scaled.is_finite() && (scaled - scaled.round()).abs() <= scaled.abs() * 1e-9 {
            return decimals;
        }
    }
    MAX_DECIMALS
}

/// Beyond twelve places an `f64` step is noise, and no axis label wants it.
pub const MAX_DECIMALS: u8 = 12;

/// Formats one value.
///
/// Never returns `NaN`, `inf` or `-0`: a non-finite value formats as an em dash,
/// which is what a missing cell should look like if one ever reaches a label.
pub fn format_value(value: f64, decimals: u8, style: NumberStyle) -> String {
    if !value.is_finite() {
        return "—".to_string();
    }
    match style {
        NumberStyle::Plain => group(&fixed(value, decimals)),
        NumberStyle::Percent => format!("{}%", group(&fixed(value * 100.0, decimals))),
        NumberStyle::Scientific => scientific(value, decimals.min(6)),
        NumberStyle::Compact => compact(value),
    }
}

/// Fixed-point, with negative zero normalised away.
fn fixed(value: f64, decimals: u8) -> String {
    let text = format!("{:.*}", decimals as usize, value);
    // `format!` on -0.0, and on a small negative rounded to zero, both give "-0".
    if text.starts_with('-') && text[1..].chars().all(|c| c == '0' || c == '.') {
        text[1..].to_string()
    } else {
        text
    }
}

/// Inserts thousands separators into the integer part of an already-formatted
/// number, leaving any sign, decimal point and suffix alone.
fn group(text: &str) -> String {
    let (sign, rest) = match text.strip_prefix('-') {
        Some(rest) => ("-", rest),
        None => ("", text),
    };
    let (integer, fraction) = match rest.split_once('.') {
        Some((integer, fraction)) => (integer, Some(fraction)),
        None => (rest, None),
    };
    let mut grouped = String::with_capacity(integer.len() + integer.len() / 3 + 2);
    for (index, digit) in integer.chars().enumerate() {
        if index > 0 && (integer.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    match fraction {
        Some(fraction) => format!("{sign}{grouped}.{fraction}"),
        None => format!("{sign}{grouped}"),
    }
}

/// `12.9K`. One decimal at most, and a trailing `.0` is dropped — `12K` beats
/// `12.0K` on an axis, where the extra glyph buys nothing.
fn compact(value: f64) -> String {
    const UNITS: [(f64, &str); 4] =
        [(1e12, "T"), (1e9, "B"), (1e6, "M"), (1e3, "K")];
    let magnitude = value.abs();
    for (threshold, suffix) in UNITS {
        if magnitude >= threshold {
            let scaled = value / threshold;
            let decimals = if scaled.abs() < 100.0 { 1 } else { 0 };
            let text = fixed(scaled, decimals);
            let text = text.strip_suffix(".0").unwrap_or(&text).to_string();
            return format!("{text}{suffix}");
        }
    }
    group(&fixed(value, if magnitude < 10.0 && magnitude != 0.0 { 1 } else { 0 }))
}

/// `1.2e-9`, with the exponent's sign shown only when negative — `1.2e9` reads
/// cleanly and `1.2e+9` reads like a spreadsheet.
fn scientific(value: f64, decimals: u8) -> String {
    if value == 0.0 {
        return "0".to_string();
    }
    let exponent = value.abs().log10().floor();
    let mantissa = value / 10_f64.powf(exponent);
    // Rounding the mantissa can carry it to ten — 9.999 at two places. Renormalise
    // *after* rounding, or the label reads "10e3" instead of "1e4". Checking the
    // unrounded mantissa is the version of this that looks correct and is not.
    let scale = 10_f64.powi(decimals as i32);
    let rounded = (mantissa * scale).round() / scale;
    let (mantissa, exponent) = if rounded.abs() >= 10.0 {
        (rounded / 10.0, exponent + 1.0)
    } else {
        (rounded, exponent)
    };
    let text = fixed(mantissa, decimals);
    let text = text.trim_end_matches('0').trim_end_matches('.').to_string();
    format!("{text}e{}", exponent as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thousands_are_grouped_and_the_sign_stays_put() {
        assert_eq!(format_value(1234.0, 0, NumberStyle::Plain), "1,234");
        assert_eq!(format_value(-1234567.0, 0, NumberStyle::Plain), "-1,234,567");
        assert_eq!(format_value(999.0, 0, NumberStyle::Plain), "999");
        assert_eq!(format_value(1000.0, 0, NumberStyle::Plain), "1,000");
        assert_eq!(format_value(-1204.5, 2, NumberStyle::Plain), "-1,204.50");
    }

    #[test]
    fn only_the_integer_part_is_grouped() {
        assert_eq!(format_value(12345.6789, 4, NumberStyle::Plain), "12,345.6789");
    }

    /// The bug this exists to prevent: a tick that rounds to zero from below must
    /// not be labelled `-0`.
    #[test]
    fn negative_zero_is_written_as_zero() {
        assert_eq!(format_value(-0.0, 0, NumberStyle::Plain), "0");
        assert_eq!(format_value(-0.0001, 2, NumberStyle::Plain), "0.00");
        assert_eq!(format_value(-0.0, 2, NumberStyle::Percent), "0.00%");
    }

    #[test]
    fn decimals_come_from_the_step_so_one_axis_has_one_shape() {
        assert_eq!(decimals_for_step(1.0), 0);
        assert_eq!(decimals_for_step(2.5), 1);
        assert_eq!(decimals_for_step(0.25), 2);
        assert_eq!(decimals_for_step(0.001), 3);
        assert_eq!(decimals_for_step(20.0), 0);
        assert_eq!(decimals_for_step(2.5e11), 0);
    }

    #[test]
    fn a_degenerate_step_asks_for_no_decimals_rather_than_looping() {
        assert_eq!(decimals_for_step(0.0), 0);
        assert_eq!(decimals_for_step(f64::NAN), 0);
        assert_eq!(decimals_for_step(f64::INFINITY), 0);
        assert_eq!(decimals_for_step(1e-30), MAX_DECIMALS);
    }

    #[test]
    fn compact_labels_drop_a_trailing_zero_decimal() {
        assert_eq!(format_value(12_000.0, 0, NumberStyle::Compact), "12K");
        assert_eq!(format_value(12_900.0, 0, NumberStyle::Compact), "12.9K");
        assert_eq!(format_value(4_200_000.0, 0, NumberStyle::Compact), "4.2M");
        assert_eq!(format_value(-1_300_000_000.0, 0, NumberStyle::Compact), "-1.3B");
        assert_eq!(format_value(2.7e12, 0, NumberStyle::Compact), "2.7T");
        assert_eq!(format_value(950.0, 0, NumberStyle::Compact), "950");
        assert_eq!(format_value(123_456.0, 0, NumberStyle::Compact), "123K");
    }

    #[test]
    fn scientific_notation_renormalises_a_carried_mantissa() {
        assert_eq!(format_value(1.2e-9, 2, NumberStyle::Scientific), "1.2e-9");
        assert_eq!(format_value(-3.0e20, 2, NumberStyle::Scientific), "-3e20");
        assert_eq!(format_value(0.0, 2, NumberStyle::Scientific), "0");
        // 9.999e3 at two decimals rounds to 10.00 — it must become 1e4, not 10e3.
        assert_eq!(format_value(9.999e3, 2, NumberStyle::Scientific), "1e4");
    }

    #[test]
    fn percentages_scale_by_a_hundred() {
        assert_eq!(format_value(0.125, 1, NumberStyle::Percent), "12.5%");
        assert_eq!(format_value(1.0, 0, NumberStyle::Percent), "100%");
    }

    #[test]
    fn a_non_finite_value_never_reaches_a_label_as_nan() {
        for style in [
            NumberStyle::Plain,
            NumberStyle::Compact,
            NumberStyle::Scientific,
            NumberStyle::Percent,
        ] {
            assert_eq!(format_value(f64::NAN, 2, style), "—");
            assert_eq!(format_value(f64::INFINITY, 2, style), "—");
        }
    }

    #[test]
    fn the_style_for_an_axis_follows_its_magnitude() {
        assert_eq!(NumberStyle::for_axis(1.0, 100.0), NumberStyle::Plain);
        assert_eq!(NumberStyle::for_axis(2e4, 5e5), NumberStyle::Compact);
        assert_eq!(NumberStyle::for_axis(1e14, 1e16), NumberStyle::Scientific);
        assert_eq!(NumberStyle::for_axis(1e-6, 1e-5), NumberStyle::Scientific);
        assert_eq!(NumberStyle::for_axis(f64::NAN, 1.0), NumberStyle::Plain);
    }
}
