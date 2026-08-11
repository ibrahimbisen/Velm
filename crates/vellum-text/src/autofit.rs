//! Auto-fit: the largest font size at which text still fits a box.
//!
//! Miro writes `"fs": 0, "fsa": 1` on a widget whose text is auto-sized —
//! `docs/02-miro-formats.md` §2.2 — and the importer turns that into
//! `TextStyle::font_size == None`. **Every sticky on the reference board is in that
//! state**, so this is not an edge case: without it, 44 of 44 stickies import at
//! whatever default the renderer picks and none of them match the SVG oracle.
//!
//! There is no closed form. Wrapping makes height a step function of font size —
//! one extra point can push a word onto a new line and add a whole line of height —
//! so the size is *searched* for, not solved.
//!
//! ## Why a binary search is sound here
//!
//! The search assumes "fits" is monotone: if the text fits at size *s*, it fits at
//! every smaller size. That is not a theorem — a pathological wrap can leave a
//! narrow box slightly happier at a larger size — but it holds for every input a
//! board contains, and the alternative (a linear scan at 0.25px granularity over a
//! 400px range) costs 1,600 shaping passes per sticky against about a dozen.
//!
//! The result is a *lower* bound: [`TextEngine::fit_font_size`] returns a size that
//! is known to fit, never one that is merely close. A sticky that overflows its own
//! border is the visible failure; one that is a quarter-point small is not.

use crate::layout::{LayoutParams, MAX_FONT_SIZE, MIN_FONT_SIZE, TextEngine};
use crate::span::StyledText;

/// The area auto-fitted text must fit inside, in layout px.
///
/// This is the widget's *inner* box: Miro insets sticky text from the note's edge,
/// and resolving that padding is the caller's job, not the text engine's.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FitBox {
    pub width: f32,
    pub height: f32,
}

impl FitBox {
    pub const fn new(width: f32, height: f32) -> Self {
        Self { width, height }
    }
}

/// Bounds and precision for the search.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AutoFit {
    /// Floor. Returned unchanged when even this overflows, so text is always laid
    /// out at a legible size and simply overflows — which is what Miro does too.
    pub min_font_size: f32,
    /// Ceiling, before the box's own height narrows it further.
    pub max_font_size: f32,
    /// The search stops once the bracket is this narrow. A quarter of a pixel is
    /// below what any renderer can show and costs about eleven shaping passes over
    /// the default range.
    pub tolerance: f32,
}

impl Default for AutoFit {
    fn default() -> Self {
        // 8px is Miro's own smallest selectable size; 400px is past the largest a
        // sticky can reach, and the box height usually binds long before it.
        Self { min_font_size: 8.0, max_font_size: 400.0, tolerance: 0.25 }
    }
}

/// Slack allowed when comparing a measured extent against the box.
///
/// Shaping accumulates advances in `f32`, so a line that exactly fills its box can
/// measure a few ten-thousandths over. Without this the search rejects the correct
/// answer and returns the size below it.
const FIT_EPSILON: f32 = 0.01;

impl TextEngine {
    /// The largest font size at which `text` fits `area`, to within
    /// [`AutoFit::tolerance`].
    ///
    /// `params.font_size` and `params.max_width` are ignored: the size is what is
    /// being searched for, and the wrap width is `area.width` by definition.
    /// Everything else — family, line height, alignment — is honoured, because all
    /// of them change where the text wraps.
    pub fn fit_font_size(
        &mut self,
        text: &StyledText,
        params: &LayoutParams,
        area: FitBox,
        options: &AutoFit,
    ) -> f32 {
        let floor = options.min_font_size.clamp(MIN_FONT_SIZE, MAX_FONT_SIZE);

        // One line at size s is `s × line_height` tall, so nothing above
        // `height / line_height` can ever fit. Deriving the ceiling from the box
        // rather than the options is what keeps the search to ~11 passes on a
        // sticky instead of ~11 on a range that is mostly hopeless.
        let line_height_factor = params.effective_line_height() / params.effective_font_size();
        let from_box = if area.height.is_finite() && area.height > 0.0 {
            area.height / line_height_factor
        } else {
            floor
        };
        let ceiling = options.max_font_size.min(from_box).clamp(floor, MAX_FONT_SIZE);

        if !self.fits(text, params, area, floor) {
            // Overflowing at the floor is a real outcome, not a failure: the text
            // is simply too long for the widget and the caller clips it.
            return floor;
        }
        if self.fits(text, params, area, ceiling) {
            return ceiling;
        }

        let tolerance = options.tolerance.max(f32::EPSILON);
        let (mut fits, mut overflows) = (floor, ceiling);
        while overflows - fits > tolerance {
            let midpoint = fits + (overflows - fits) / 2.0;
            // Guard against the bracket stalling on a midpoint that rounds to an
            // endpoint, which f32 can do once the interval is tiny.
            if midpoint <= fits || midpoint >= overflows {
                break;
            }
            if self.fits(text, params, area, midpoint) {
                fits = midpoint;
            } else {
                overflows = midpoint;
            }
        }
        fits
    }

    /// Whether `text` at `font_size` stays inside `area` when wrapped to its width.
    ///
    /// The width test is not redundant with wrapping: `Wrap::WordOrGlyph` breaks
    /// inside a word only as a last resort, and a single glyph wider than the box
    /// still overflows it.
    fn fits(
        &mut self,
        text: &StyledText,
        params: &LayoutParams,
        area: FitBox,
        font_size: f32,
    ) -> bool {
        let probe = params.with_font_size(font_size).with_max_width(Some(area.width));
        let extent = self.measure(text, &probe);
        extent.width <= area.width + FIT_EPSILON && extent.height <= area.height + FIT_EPSILON
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::tests::engine;
    use crate::span::{SpanStyle, TextSpan};

    fn sticky_params() -> LayoutParams {
        // The reference board's stickies: `ffn: "Noto Sans"`, `ta: "c"`, `lh: 1.36`.
        LayoutParams { line_height: 1.36, ..LayoutParams::default() }
    }

    /// The contract: the size returned fits, and the next quantum up does not.
    /// Everything else about auto-fit is a detail; this is the whole promise.
    #[test]
    fn the_result_fits_and_is_maximal_to_within_the_tolerance() {
        let text = StyledText::plain("The quick brown fox jumps over the lazy dog");
        let area = FitBox::new(200.0, 120.0);
        let options = AutoFit::default();
        let params = sticky_params();
        let mut engine = engine();

        let size = engine.fit_font_size(&text, &params, area, &options);
        assert!(size > options.min_font_size, "the box is roomy enough to grow into");

        let fitted = engine.measure(&text, &params.with_font_size(size).with_max_width(Some(area.width)));
        assert!(fitted.width <= area.width + 0.01, "{fitted:?}");
        assert!(fitted.height <= area.height + 0.01, "{fitted:?}");

        let over = size + options.tolerance * 2.0;
        let overflowed =
            engine.measure(&text, &params.with_font_size(over).with_max_width(Some(area.width)));
        assert!(
            overflowed.width > area.width + 0.01 || overflowed.height > area.height + 0.01,
            "size {size} was not maximal: {over} still fits as {overflowed:?}"
        );
    }

    /// A bigger box must never fit a smaller size — the property that makes the
    /// search meaningful at all.
    #[test]
    fn a_larger_box_never_yields_a_smaller_size() {
        let text = StyledText::plain("Sensors and actuators");
        let params = sticky_params();
        let options = AutoFit::default();
        let mut engine = engine();
        let mut previous = 0.0;
        for scale in [1.0, 1.5, 2.0, 4.0, 8.0] {
            let size = engine.fit_font_size(
                &text,
                &params,
                FitBox::new(100.0 * scale, 60.0 * scale),
                &options,
            );
            assert!(size >= previous, "shrank from {previous} to {size} at scale {scale}");
            previous = size;
        }
    }

    /// More text in the same box must not be set larger.
    #[test]
    fn more_text_never_yields_a_larger_size() {
        let params = sticky_params();
        let options = AutoFit::default();
        let area = FitBox::new(180.0, 180.0);
        let mut engine = engine();
        let short = engine.fit_font_size(&StyledText::plain("fan"), &params, area, &options);
        let long = engine.fit_font_size(
            &StyledText::plain("fan control module, rear left, harness side"),
            &params,
            area,
            &options,
        );
        assert!(long < short, "short {short} long {long}");
    }

    #[test]
    fn text_that_cannot_fit_returns_the_floor_rather_than_failing() {
        let text = StyledText::plain("overflowing ".repeat(200));
        let options = AutoFit::default();
        let size =
            engine().fit_font_size(&text, &sticky_params(), FitBox::new(40.0, 20.0), &options);
        assert_eq!(size, options.min_font_size);
    }

    /// A single line cannot be taller than the box, which is where the ceiling
    /// comes from. Without it the search would spend its budget above any size
    /// that could possibly fit.
    #[test]
    fn the_ceiling_comes_from_the_box_height() {
        let params = LayoutParams { line_height: 1.0, ..LayoutParams::default() };
        let options = AutoFit { max_font_size: 10_000.0, ..AutoFit::default() };
        // One short word in a wide, 50px-tall box: height binds, not width.
        let size =
            engine().fit_font_size(&StyledText::plain("a"), &params, FitBox::new(5000.0, 50.0), &options);
        assert!(size <= 50.0 + options.tolerance, "{size}");
        assert!(size > 40.0, "{size} should be close to the 50px ceiling");
    }

    /// Empty text has no glyphs to bound it, so it fills the box up to the ceiling.
    /// The caret in an empty sticky is then the size the first typed character
    /// will be, which is what Miro shows.
    #[test]
    fn empty_text_takes_the_ceiling() {
        let params = LayoutParams { line_height: 1.0, ..LayoutParams::default() };
        let options = AutoFit { max_font_size: 60.0, ..AutoFit::default() };
        let size =
            engine().fit_font_size(&StyledText::default(), &params, FitBox::new(200.0, 200.0), &options);
        assert_eq!(size, 60.0);
    }

    /// A degenerate box must return a legible floor rather than zero or NaN.
    #[test]
    fn degenerate_boxes_degrade_to_the_floor() {
        let text = StyledText::plain("fan");
        let params = sticky_params();
        let options = AutoFit::default();
        let mut engine = engine();
        for area in [
            FitBox::new(0.0, 0.0),
            FitBox::new(-10.0, -10.0),
            FitBox::new(f32::NAN, f32::NAN),
            FitBox::new(200.0, 0.0),
        ] {
            let size = engine.fit_font_size(&text, &params, area, &options);
            assert_eq!(size, options.min_font_size, "{area:?}");
        }
    }

    /// Hard line breaks bind the height, so the verified sticky body — which ends
    /// in one — must be set smaller than the same word without it.
    #[test]
    fn the_blank_line_in_the_verified_sticky_costs_font_size() {
        let params = sticky_params();
        let options = AutoFit::default();
        let area = FitBox::new(199.0, 228.0); // the real sticky's size
        let mut engine = engine();
        let with_blank = engine.fit_font_size(
            &crate::from_miro_html("<p>fan</p><p><br /></p>"),
            &params,
            area,
            &options,
        );
        let without = engine.fit_font_size(&crate::from_miro_html("<p>fan</p>"), &params, area, &options);
        assert!(with_blank < without, "with {with_blank} without {without}");
    }

    /// Bold text is wider, so it must not be auto-fitted as if it were regular.
    #[test]
    fn span_formatting_participates_in_the_fit() {
        let params = sticky_params();
        let options = AutoFit::default();
        let area = FitBox::new(150.0, 60.0);
        let mut engine = engine();
        let regular = engine.fit_font_size(&StyledText::plain("nnnnnnnnnn"), &params, area, &options);
        let bold = engine.fit_font_size(
            &StyledText::from_spans([TextSpan::new("nnnnnnnnnn", SpanStyle::bold())]),
            &params,
            area,
            &options,
        );
        assert!(bold <= regular, "regular {regular} bold {bold}");
    }

    /// Options that make no sense must not produce a NaN or an infinite loop.
    #[test]
    fn inverted_and_zero_bounds_terminate() {
        let text = StyledText::plain("fan");
        let params = sticky_params();
        let mut engine = engine();
        let inverted = AutoFit { min_font_size: 100.0, max_font_size: 10.0, tolerance: 0.25 };
        let size = engine.fit_font_size(&text, &params, FitBox::new(300.0, 300.0), &inverted);
        assert!(size.is_finite() && size > 0.0, "{size}");

        let zero_tolerance = AutoFit { tolerance: 0.0, ..AutoFit::default() };
        let size = engine.fit_font_size(&text, &params, FitBox::new(300.0, 300.0), &zero_tolerance);
        assert!(size.is_finite() && size > 0.0, "{size}");
    }
}
