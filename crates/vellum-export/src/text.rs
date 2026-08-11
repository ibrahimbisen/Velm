//! Text as the exporter receives it: **already laid out into lines**.
//!
//! This is the single most important boundary in the crate, so the reasoning is
//! recorded rather than assumed.
//!
//! Shaping and wrapping belong to `vellum-text` and its `cosmic-text` backend.
//! `docs/01-architecture.md` §6 calls text the project's largest risk and budgets
//! 4–8 weeks for it; an exporter that re-implemented line breaking would produce a
//! *second*, subtly different layout, and every export would then disagree with the
//! canvas the user was looking at. Wrap-width parity with Miro is hard enough to
//! achieve once.
//!
//! So a [`TextBlock`] is a list of [`TextLine`]s with baselines the caller already
//! computed. The exporter's job is placement and encoding: put each line where the
//! caller says, honour alignment, escape it correctly, and — for PDF — turn it into
//! glyphs. For callers that genuinely have no shaper (a test, a CSV-only export),
//! [`TextBlock::plain`] splits on newlines and spaces baselines by line height,
//! which is exactly as good as it claims to be and no better.

use crate::style::Color;

/// Canvas line height, from `docs/05-design-language.md` §5 — chosen to match
/// Miro's default so imported boards do not reflow.
pub const CANVAS_LINE_HEIGHT: f64 = 1.36;

/// Fraction of the em box below the baseline, used only by [`TextBlock::plain`] to
/// place the first baseline. Real metrics come from the font when one is available.
const NOMINAL_DESCENT: f64 = 0.22;

/// Which font a run wants. Resolution is the caller's problem: `vellum-text` owns
/// the font database, and this is the request it answers.
#[derive(Debug, Clone, PartialEq)]
pub struct FontSpec {
    /// Family name as it should appear in the output, e.g. `"Noto Sans"`.
    pub family: String,
    /// Size in board units.
    pub size: f64,
    /// CSS numeric weight, 100–900. 400 is regular, 700 bold.
    pub weight: u16,
    pub italic: bool,
}

impl FontSpec {
    pub fn new(family: impl Into<String>, size: f64) -> Self {
        Self { family: family.into(), size, weight: 400, italic: false }
    }

    pub fn bold(mut self) -> Self {
        self.weight = 700;
        self
    }

    pub fn italic(mut self) -> Self {
        self.italic = true;
        self
    }

    /// The identity a font *file* is looked up by. Size is not part of it: one file
    /// serves every size, and keying on size would embed the same font repeatedly.
    pub fn face_key(&self) -> FaceKey {
        FaceKey { family: self.family.clone(), weight: self.weight, italic: self.italic }
    }
}

impl Default for FontSpec {
    /// Inter at 14pt — the bundled canvas default from `docs/05-design-language.md`.
    fn default() -> Self {
        Self::new("Inter", 14.0)
    }
}

/// A font file's identity, independent of the size it is used at.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FaceKey {
    pub family: String,
    pub weight: u16,
    pub italic: bool,
}

/// A styled run within one line. A run never contains a line break.
#[derive(Debug, Clone, PartialEq)]
pub struct Span {
    pub text: String,
    pub font: FontSpec,
    pub color: Color,
    pub underline: bool,
    pub strikethrough: bool,
    /// A link on the run itself, as distinct from a link on the whole item.
    pub link: Option<String>,
}

impl Span {
    pub fn new(text: impl Into<String>, font: FontSpec, color: Color) -> Self {
        Self { text: text.into(), font, color, underline: false, strikethrough: false, link: None }
    }
}

/// One visual line, positioned by its baseline.
#[derive(Debug, Clone, PartialEq)]
pub struct TextLine {
    /// Baseline distance **downwards from the top of the text box**, in board
    /// units. Baselines rather than line tops because that is what both SVG's
    /// `<text y>` and PDF's `Td` want, and converting the other way needs font
    /// metrics the exporter may not have.
    pub baseline: f64,
    pub spans: Vec<Span>,
}

impl TextLine {
    pub fn new(baseline: f64, spans: Vec<Span>) -> Self {
        Self { baseline, spans }
    }

    /// The line's characters with no styling, for CSV and for measurement.
    pub fn plain_text(&self) -> String {
        self.spans.iter().map(|s| s.text.as_str()).collect()
    }

    pub fn is_blank(&self) -> bool {
        self.spans.iter().all(|s| s.text.trim().is_empty())
    }

    /// The tallest font on the line, which is what governs its visual height.
    pub(crate) fn dominant_font(&self) -> Option<&FontSpec> {
        self.spans.iter().map(|s| &s.font).max_by(|a, b| a.size.total_cmp(&b.size))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Align {
    #[default]
    Left,
    Center,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VAlign {
    Top,
    #[default]
    Middle,
    Bottom,
}

/// The text on an item.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct TextBlock {
    pub lines: Vec<TextLine>,
    pub align: Align,
    pub valign: VAlign,
    /// Inset from the item's rect on all four sides. Miro's stickies use one.
    pub padding: f64,
}

impl TextBlock {
    /// A block from lines the caller has already laid out.
    pub fn new(lines: Vec<TextLine>) -> Self {
        Self { lines, ..Self::default() }
    }

    /// Splits `text` on newlines and spaces the baselines by line height.
    ///
    /// **This is not layout.** There is no wrapping, no shaping, no kerning and no
    /// bidi. It exists so a caller without a shaper — a test, a CSV export, a
    /// headless thumbnail — can still produce output, and so that the rest of the
    /// crate has one construction path to exercise. Anything user-facing should
    /// pass lines from `vellum-text`.
    pub fn plain(text: &str, font: FontSpec, color: Color) -> Self {
        let step = font.size * CANVAS_LINE_HEIGHT;
        let first = step - font.size * NOMINAL_DESCENT;
        let lines = text
            .split('\n')
            .enumerate()
            .map(|(i, line)| {
                TextLine::new(
                    first + step * i as f64,
                    vec![Span::new(line.trim_end_matches('\r'), font.clone(), color)],
                )
            })
            .collect();
        Self::new(lines)
    }

    pub fn with_align(mut self, align: Align) -> Self {
        self.align = align;
        self
    }

    pub fn with_valign(mut self, valign: VAlign) -> Self {
        self.valign = valign;
        self
    }

    pub fn with_padding(mut self, padding: f64) -> Self {
        self.padding = padding;
        self
    }

    /// Every line joined by `\n`, which is what CSV and search want.
    pub fn plain_text(&self) -> String {
        self.lines.iter().map(TextLine::plain_text).collect::<Vec<_>>().join("\n")
    }

    /// True when the block would put no glyphs on the page. Blocks like this are
    /// dropped rather than written, because an empty `<text>` element still counts
    /// as a text element to anything reading the output back.
    pub fn is_empty(&self) -> bool {
        self.lines.iter().all(TextLine::is_blank)
    }

    /// Distance from the first line's baseline to the last's — the block's own
    /// height, used to resolve vertical alignment inside the item rect.
    pub(crate) fn baseline_span(&self) -> f64 {
        match (self.lines.first(), self.lines.last()) {
            (Some(first), Some(last)) => last.baseline - first.baseline,
            _ => 0.0,
        }
    }

    /// How far every baseline shifts to satisfy [`VAlign`] inside a box of
    /// `box_height`, padding already removed.
    ///
    /// `Top` is the identity: the caller's baselines are measured from the top of
    /// the box, so they are already correct. The other two need the block's own
    /// extent, which is why [`baseline_span`](Self::baseline_span) exists.
    pub(crate) fn valign_offset(&self, box_height: f64) -> f64 {
        let Some(first) = self.lines.first() else { return 0.0 };
        let ascent = first.baseline;
        let descent = first.dominant_font().map_or(0.0, |f| f.size * NOMINAL_DESCENT);
        let block = ascent + self.baseline_span() + descent;
        match self.valign {
            VAlign::Top => 0.0,
            VAlign::Middle => (box_height - block) / 2.0,
            VAlign::Bottom => box_height - block,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn font() -> FontSpec {
        FontSpec::new("Inter", 10.0)
    }

    #[test]
    fn plain_splits_on_newlines_and_steps_baselines_by_line_height() {
        let block = TextBlock::plain("one\ntwo\nthree", font(), Color::BLACK);
        assert_eq!(block.lines.len(), 3);
        let step = block.lines[1].baseline - block.lines[0].baseline;
        assert!((step - 10.0 * CANVAS_LINE_HEIGHT).abs() < 1e-9, "{step}");
        assert_eq!(block.plain_text(), "one\ntwo\nthree");
    }

    #[test]
    fn plain_strips_carriage_returns_from_crlf_text() {
        let block = TextBlock::plain("one\r\ntwo", font(), Color::BLACK);
        assert_eq!(block.plain_text(), "one\ntwo");
    }

    #[test]
    fn whitespace_only_text_counts_as_empty() {
        assert!(TextBlock::plain("   \n\t", font(), Color::BLACK).is_empty());
        assert!(!TextBlock::plain(" x ", font(), Color::BLACK).is_empty());
        assert!(TextBlock::default().is_empty());
    }

    #[test]
    fn the_dominant_font_is_the_largest_on_the_line() {
        let line = TextLine::new(
            10.0,
            vec![
                Span::new("small", FontSpec::new("Inter", 8.0), Color::BLACK),
                Span::new("BIG", FontSpec::new("Inter", 24.0), Color::BLACK),
            ],
        );
        assert_eq!(line.dominant_font().map(|f| f.size), Some(24.0));
    }

    #[test]
    fn top_alignment_leaves_the_callers_baselines_untouched() {
        let block = TextBlock::plain("a\nb", font(), Color::BLACK).with_valign(VAlign::Top);
        assert_eq!(block.valign_offset(500.0), 0.0);
    }

    #[test]
    fn middle_and_bottom_alignment_move_by_the_leftover_space() {
        let block = TextBlock::plain("a\nb", font(), Color::BLACK);
        let middle = block.clone().with_valign(VAlign::Middle).valign_offset(100.0);
        let bottom = block.with_valign(VAlign::Bottom).valign_offset(100.0);
        assert!(middle > 0.0 && bottom > middle, "middle {middle}, bottom {bottom}");
        // Middle is exactly halfway between top and bottom.
        assert!((bottom / 2.0 - middle).abs() < 1e-9);
    }

    #[test]
    fn a_face_is_keyed_without_its_size() {
        let a = FontSpec::new("Inter", 12.0).bold();
        let b = FontSpec::new("Inter", 96.0).bold();
        assert_eq!(a.face_key(), b.face_key());
        assert_ne!(a.face_key(), FontSpec::new("Inter", 12.0).face_key());
    }
}
