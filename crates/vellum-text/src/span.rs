//! Styled text: an ordered, normalised list of formatted runs.
//!
//! These types are a deliberate, documented **duplicate** of
//! `vellum_doc::text::{SpanStyle, TextSpan, StyledText}`. The dependency direction
//! in `docs/01-architecture.md` §2 is `doc → text`, so depending back on
//! `vellum-doc` would close a cycle, and this crate must also be usable by the
//! importer and the renderer without dragging a CRDT in.
//!
//! The correspondence is one-to-one, so the conversion at the `vellum-doc`
//! boundary is a `map`:
//!
//! | here | `vellum_doc::text` | difference |
//! |---|---|---|
//! | [`SpanStyle`] | `SpanStyle` | none — same six fields, same meanings |
//! | [`TextSpan`] | `TextSpan` | none |
//! | [`StyledText`] | `StyledText` | none — same normalisation invariant |
//! | [`Rgb`] | `Color` | `Color` carries alpha; Miro's rich-text HTML never does, so a span colour is always opaque here |
//!
//! Keeping the invariant identical is the point: `StyledText::from_spans` drops
//! empty spans and merges adjacent runs of equal style on *both* sides, so a value
//! that survives HTML → text → document → text compares equal at every hop. Two
//! different normalisations would make round-trip tests quietly meaningless.

/// An opaque sRGB colour, as it appears in a `style="color: …"` attribute.
///
/// No alpha: CSS `rgba()` alpha is parsed and discarded rather than rejected,
/// because a translucent glyph run is not something Miro's editor can produce and
/// silently dropping the tag entirely would lose the colour as well.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    /// CSS-style hex, matching the form `vellum_doc::Color::to_hex` emits for an
    /// opaque colour so the two can be compared directly in tests.
    pub fn to_hex(self) -> String {
        format!("#{:02x}{:02x}{:02x}", self.r, self.g, self.b)
    }
}

/// Character-level formatting. Everything here can vary *within* one text run.
///
/// Font family, size, alignment and line height are deliberately absent: Miro
/// carries them as widget-level style keys (`ffn`, `fs`, `ta`, `lh`) and so does
/// Vellum — see [`LayoutParams`](crate::LayoutParams). Duplicating them per span
/// would create two sources of truth for the same property.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SpanStyle {
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strikethrough: bool,
    /// Target of a hyperlink covering this span.
    pub link: Option<String>,
    /// Overrides the item's text colour for this span.
    pub color: Option<Rgb>,
}

impl SpanStyle {
    /// Unformatted text.
    pub fn plain() -> Self {
        Self::default()
    }

    pub fn bold() -> Self {
        Self { bold: true, ..Self::default() }
    }

    pub fn italic() -> Self {
        Self { italic: true, ..Self::default() }
    }

    pub fn link(url: impl Into<String>) -> Self {
        Self { link: Some(url.into()), ..Self::default() }
    }

    /// True when this run needs no formatting recorded anywhere.
    pub fn is_plain(&self) -> bool {
        self == &Self::default()
    }
}

/// A run of text sharing one [`SpanStyle`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextSpan {
    pub text: String,
    pub style: SpanStyle,
}

impl TextSpan {
    pub fn new(text: impl Into<String>, style: SpanStyle) -> Self {
        Self { text: text.into(), style }
    }

    pub fn plain(text: impl Into<String>) -> Self {
        Self { text: text.into(), style: SpanStyle::default() }
    }
}

/// A styled text value: an ordered, normalised list of spans.
///
/// Normalisation (empty spans dropped, adjacent same-style spans merged) happens on
/// construction so that two `StyledText`s representing the same visible text are
/// always `==`. Without it the HTML converter's output would depend on incidental
/// tag structure — `<b>a</b><b>b</b>` and `<b>ab</b>` would compare unequal while
/// rendering identically — which would make every assertion in this crate a
/// statement about the parser's internals rather than about the text.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StyledText {
    spans: Vec<TextSpan>,
}

impl StyledText {
    /// Unformatted text — the common case while the editor is plain-text only.
    pub fn plain(text: impl Into<String>) -> Self {
        Self::from_spans([TextSpan::plain(text)])
    }

    pub fn from_spans(spans: impl IntoIterator<Item = TextSpan>) -> Self {
        let mut out: Vec<TextSpan> = Vec::new();
        for span in spans {
            if span.text.is_empty() {
                continue;
            }
            match out.last_mut() {
                Some(prev) if prev.style == span.style => prev.text.push_str(&span.text),
                _ => out.push(span),
            }
        }
        Self { spans: out }
    }

    pub fn spans(&self) -> &[TextSpan] {
        &self.spans
    }

    /// The text with all formatting dropped — what shaping, a plain-text editor and
    /// a search index all want.
    pub fn to_plain(&self) -> String {
        self.spans.iter().map(|s| s.text.as_str()).collect()
    }

    pub fn is_empty(&self) -> bool {
        self.spans.is_empty()
    }

    /// Length in Unicode scalar values, which is the unit Loro indexes text by.
    pub fn char_len(&self) -> usize {
        self.spans.iter().map(|s| s.text.chars().count()).sum()
    }

    /// Number of visual lines *before* wrapping — one more than the number of
    /// hard breaks. Layout reports the wrapped count; this is the floor.
    pub fn hard_line_count(&self) -> usize {
        1 + self.spans.iter().map(|s| s.text.matches('\n').count()).sum::<usize>()
    }
}

impl From<&str> for StyledText {
    fn from(s: &str) -> Self {
        Self::plain(s)
    }
}

impl From<String> for StyledText {
    fn from(s: String) -> Self {
        Self::plain(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Normalisation is what makes `StyledText` equality meaningful, so it is
    /// tested directly rather than only through the HTML converter.
    #[test]
    fn adjacent_spans_with_equal_style_merge() {
        let t = StyledText::from_spans([
            TextSpan::plain("Hello, "),
            TextSpan::plain("world"),
            TextSpan::new("!", SpanStyle::bold()),
        ]);
        assert_eq!(t.spans().len(), 2);
        assert_eq!(t.spans()[0].text, "Hello, world");
        assert_eq!(t.to_plain(), "Hello, world!");
    }

    #[test]
    fn empty_spans_are_dropped() {
        let t = StyledText::from_spans([
            TextSpan::plain(""),
            TextSpan::new("", SpanStyle::bold()),
            TextSpan::plain("x"),
        ]);
        assert_eq!(t.spans().len(), 1);
        assert!(!t.is_empty());
        assert!(StyledText::plain("").is_empty());
    }

    /// Mark ranges are computed in chars on the document side, so the length this
    /// crate reports must be in the same unit. Getting it wrong only shows up on
    /// non-ASCII text.
    #[test]
    fn char_len_counts_scalars_not_bytes() {
        let t = StyledText::plain("héllo wörld");
        assert_eq!(t.char_len(), 11);
        assert_eq!(t.to_plain().len(), 13);
    }

    #[test]
    fn hard_line_count_counts_breaks_across_spans() {
        let t = StyledText::from_spans([
            TextSpan::new("a\n", SpanStyle::bold()),
            TextSpan::plain("b\nc"),
        ]);
        assert_eq!(t.hard_line_count(), 3);
        assert_eq!(StyledText::default().hard_line_count(), 1);
    }

    #[test]
    fn colours_render_as_the_same_hex_the_svg_export_uses() {
        assert_eq!(Rgb::new(0xFF, 0xF7, 0x9E).to_hex(), "#fff79e");
    }
}
