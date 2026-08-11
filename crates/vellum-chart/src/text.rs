//! How wide a label is — asked, never assumed.
//!
//! Two of this crate's jobs are impossible without measuring text: reserving the
//! left axis band (as wide as its widest tick label, no wider) and deciding whether
//! a value label fits inside its bar. Both are *layout* decisions, so they have to
//! happen here, in a crate that has no fonts and no shaper.
//!
//! The way out is to take measurement as a parameter. `vellum-text` owns
//! `cosmic-text` and can answer exactly; this crate only needs to ask. That keeps
//! the dependency direction pointing the right way — a geometry crate that pulled in
//! a shaper would drag font loading into every unit test.
//!
//! # Why the built-in estimate is not a cop-out for numbers
//!
//! `docs/05-design-language.md` §3 requires **monospace with tabular figures for
//! anything numeric**, which is every tick label and every value label a chart
//! draws. In a monospaced face a fixed advance per character *is* the exact width,
//! so [`MonoMetrics`] is not an approximation for the labels that matter — it is the
//! right answer, and a chart laid out with it and rendered with SF Mono lines up.
//!
//! Category labels are proportional UI text, and there [`MonoMetrics`] is a genuine
//! estimate: it will over-reserve for `Iiil` and under-reserve for `WWW`. Callers
//! that have a shaper should pass it. The default is chosen to be *conservative* —
//! it errs wide, so a label that would have collided is dropped rather than drawn
//! overlapping.

/// Measures a run of text at a size, in the chart's own units.
///
/// Implementations must be pure: the layout runs twice (see
/// [`crate::chart::build`]) and two different answers for the same string would make
/// the second pass disagree with the first.
pub trait TextMetrics {
    /// The advance width of `text` at `size`, ignoring line breaks — chart labels
    /// are single-line by construction.
    fn width(&self, text: &str, size: f32) -> f32;

    /// The distance from one baseline to the next, which is what the crate uses as
    /// a label's box height. Design language §5: 1.4 for UI text.
    fn line_height(&self, size: f32) -> f32 {
        size * 1.4
    }
}

/// The default: a fixed advance per character.
///
/// Exact for the monospaced numerals every axis is labelled with, deliberately
/// generous for proportional text. `0.6` is SF Mono's advance ratio, and Cascadia
/// Mono's is the same to two decimal places, so one constant serves both platforms
/// named in the design language.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MonoMetrics {
    /// Advance width as a fraction of the font size.
    pub advance_ratio: f32,
    /// Line height as a fraction of the font size.
    pub line_height_ratio: f32,
}

impl MonoMetrics {
    pub const fn new(advance_ratio: f32, line_height_ratio: f32) -> Self {
        Self { advance_ratio, line_height_ratio }
    }
}

impl Default for MonoMetrics {
    fn default() -> Self {
        Self::new(0.6, 1.4)
    }
}

impl TextMetrics for MonoMetrics {
    fn width(&self, text: &str, size: f32) -> f32 {
        // `chars`, not `len`: a label may carry a minus sign, a thin space or a
        // multiplication dot, and counting UTF-8 bytes would triple-charge them.
        text.chars().count() as f32 * self.advance_ratio * size
    }

    fn line_height(&self, size: f32) -> f32 {
        size * self.line_height_ratio
    }
}

/// The single-character ellipsis used when a label is shortened. One glyph rather
/// than three periods, so the truncation costs a third of the width.
pub const ELLIPSIS: &str = "…";

/// Shortens `text` until it fits `max_width`, ending in an ellipsis.
///
/// Returns `None` when not even the ellipsis fits, which is the caller's cue to draw
/// nothing at all — a clipped label is worse than an absent one, because it crops
/// the characters that identify it. Truncation happens on `char` boundaries, so a
/// multi-byte label cannot be cut into invalid UTF-8.
pub fn truncate_to_width(
    text: &str,
    size: f32,
    max_width: f32,
    metrics: &dyn TextMetrics,
) -> Option<String> {
    if metrics.width(text, size) <= max_width {
        return Some(text.to_string());
    }
    if metrics.width(ELLIPSIS, size) > max_width {
        return None;
    }
    // Walk back one character at a time. Labels are short — a category name or a
    // formatted number — so the quadratic worst case is a handful of measurements,
    // and it is exact for proportional fonts where a width-per-char estimate is not.
    let mut end = text.len();
    while end > 0 {
        let candidate = &text[..end];
        let shortened = format!("{candidate}{ELLIPSIS}");
        if metrics.width(&shortened, size) <= max_width {
            return Some(shortened);
        }
        end = text[..end]
            .char_indices()
            .next_back()
            .map(|(index, _)| index)
            .unwrap_or(0);
    }
    Some(ELLIPSIS.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_monospaced_measure_is_exact_for_numerals() {
        let metrics = MonoMetrics::default();
        assert!((metrics.width("1,234", 10.0) - 30.0).abs() < 1e-5);
        assert_eq!(metrics.width("", 10.0), 0.0);
    }

    #[test]
    fn width_counts_characters_not_utf8_bytes() {
        let metrics = MonoMetrics::default();
        // "−12°" is four characters and seven bytes.
        assert_eq!(metrics.width("−12°", 10.0), metrics.width("ab12", 10.0));
    }

    #[test]
    fn truncation_ends_in_an_ellipsis_and_never_exceeds_the_budget() {
        let metrics = MonoMetrics::default();
        // Eight characters at 6px each is 48; the budget of 40 fits five plus the
        // ellipsis, and not six.
        let fit = truncate_to_width("Assembly", 10.0, 40.0, &metrics).unwrap();
        assert_eq!(fit, "Assem…");
        assert!(metrics.width(&fit, 10.0) <= 40.0);
    }

    #[test]
    fn text_that_already_fits_is_returned_untouched() {
        let metrics = MonoMetrics::default();
        assert_eq!(truncate_to_width("Q1", 10.0, 60.0, &metrics).as_deref(), Some("Q1"));
    }

    #[test]
    fn a_budget_too_small_for_the_ellipsis_yields_nothing_rather_than_a_crop() {
        let metrics = MonoMetrics::default();
        assert_eq!(truncate_to_width("Assembly", 10.0, 3.0, &metrics), None);
        assert_eq!(truncate_to_width("Assembly", 10.0, 0.0, &metrics), None);
    }

    #[test]
    fn truncation_splits_on_character_boundaries() {
        let metrics = MonoMetrics::default();
        // Every character is three bytes; a byte-wise cut would panic or corrupt.
        let out = truncate_to_width("шасси-номер", 10.0, 30.0, &metrics).unwrap();
        assert!(out.ends_with(ELLIPSIS));
        assert!(metrics.width(&out, 10.0) <= 30.0);
    }
}
