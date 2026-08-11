//! Styled text in a cell: an ordered, normalised list of formatted runs.
//!
//! These types are a deliberate, documented **duplicate** of
//! `vellum_text::span::{Rgb, SpanStyle, TextSpan, StyledText}`, which is itself a
//! documented duplicate of `vellum_doc::text`. The reason is the same one that crate
//! gives: the dependency direction in `docs/01-architecture.md` §2 runs downward,
//! and a pure-geometry crate that pulled in `cosmic-text` — 40-odd transitive crates,
//! a font database and a shaping engine — to name six booleans would make `cargo
//! test` on the table model wait for a text stack it never calls.
//!
//! The correspondence is one-to-one, so the conversion at either boundary is a
//! `map`:
//!
//! | here | `vellum_text::span` | difference |
//! |---|---|---|
//! | [`Rgb`] | `Rgb` | none — same three `u8` channels, same `to_hex` |
//! | [`SpanStyle`] | `SpanStyle` | none — same six fields, same meanings |
//! | [`TextSpan`] | `TextSpan` | none |
//! | [`StyledText`] | `StyledText` | none in behaviour; this copy derives `Serialize`/`Deserialize`, because a table cell is persisted as part of the widget and `vellum-text`'s copy is a transient input to shaping |
//!
//! **The normalisation invariant is identical and must stay identical**: empty spans
//! dropped, adjacent runs of equal style merged, on construction. That is what makes
//! `==` meaningful across the hop — a cell that survives HTML → text → table →
//! document → table compares equal at every stage. Two different normalisations
//! would make every round-trip test in the tree quietly vacuous.
//!
//! Cell-level formatting — family, size, line height, horizontal alignment — is
//! **not** here, for the same reason it is not in `vellum-text`'s copy: Miro carries
//! it as widget-level style keys and so does Vellum. It lives on
//! [`TextStyle`](crate::TextStyle), one level up, where a header row can set it once
//! for a whole row instead of rewriting every span underneath it.

use serde::{Deserialize, Serialize};

/// An opaque sRGB colour, as it appears in a `style="color: …"` attribute.
///
/// No alpha, matching `vellum_text::Rgb`: Miro's rich-text HTML never carries a
/// translucent glyph run, and a span colour that could be half-transparent would
/// have to be composited against the cell fill during layout rather than at draw
/// time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    /// CSS-style hex, matching the form `vellum_text::Rgb::to_hex` emits so the two
    /// can be compared directly in a cross-crate test.
    pub fn to_hex(self) -> String {
        format!("#{:02x}{:02x}{:02x}", self.r, self.g, self.b)
    }
}

/// Character-level formatting. Everything here can vary *within* one cell.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpanStyle {
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strikethrough: bool,
    /// Target of a hyperlink covering this span.
    pub link: Option<String>,
    /// Overrides the cell's text colour for this span.
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

/// A cell's text: an ordered, normalised list of spans.
///
/// Normalisation happens on construction so that two values representing the same
/// visible text are always `==`. See the module docs for why that invariant has to
/// match `vellum-text`'s exactly.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
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

    /// The text with all formatting dropped — what measurement, a plain-text editor
    /// and a search index all want.
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

    /// Concatenation, used when a merge folds several cells into one. Normalisation
    /// runs over the join, so merging `"a"` into `"b"` yields one span rather than
    /// two — otherwise a merge would silently change what the cell compares equal to.
    pub fn concat(parts: impl IntoIterator<Item = Self>, separator: &str) -> Self {
        let mut spans: Vec<TextSpan> = Vec::new();
        for part in parts {
            if part.is_empty() {
                continue;
            }
            if !spans.is_empty() && !separator.is_empty() {
                spans.push(TextSpan::plain(separator));
            }
            spans.extend(part.spans);
        }
        Self::from_spans(spans)
    }

    /// Applies cell-level weight, slant and colour to every span that does not
    /// already override them.
    ///
    /// This is how a header row is bolded without rewriting its text.
    /// `vellum-text` — and Miro's HTML before it — carry weight *per span*, while a
    /// header is a property of the row, so somebody has to fold one into the other.
    /// Doing it here, on the way into layout, means the stored spans still say what
    /// the user typed: un-marking the header row restores plain text instead of
    /// leaving every cell permanently bold.
    ///
    /// Colour is applied only where a span has none, because an explicitly coloured
    /// run is a decision the user made about that run specifically.
    pub fn with_cell_defaults(&self, bold: bool, italic: bool, color: Option<Rgb>) -> Self {
        if !bold && !italic && color.is_none() {
            return self.clone();
        }
        Self::from_spans(self.spans.iter().map(|span| TextSpan {
            text: span.text.clone(),
            style: SpanStyle {
                bold: span.style.bold || bold,
                italic: span.style.italic || italic,
                color: span.style.color.or(color),
                ..span.style.clone()
            },
        }))
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

    #[test]
    fn adjacent_spans_with_equal_style_merge() {
        let t = StyledText::from_spans([
            TextSpan::plain("Total"),
            TextSpan::plain(": "),
            TextSpan::new("12", SpanStyle::bold()),
        ]);
        assert_eq!(t.spans().len(), 2);
        assert_eq!(t.spans()[0].text, "Total: ");
        assert_eq!(t.to_plain(), "Total: 12");
    }

    #[test]
    fn empty_spans_are_dropped() {
        let t = StyledText::from_spans([
            TextSpan::plain(""),
            TextSpan::new("", SpanStyle::bold()),
            TextSpan::plain("x"),
        ]);
        assert_eq!(t.spans().len(), 1);
        assert!(StyledText::plain("").is_empty());
    }

    /// A merge folds several cells into one. If the join were not renormalised, the
    /// merged cell would hold two identically-styled spans and stop comparing equal
    /// to the same text typed directly.
    #[test]
    fn concatenation_renormalises_across_the_join() {
        let joined = StyledText::concat(
            [StyledText::plain("a"), StyledText::default(), StyledText::plain("b")],
            "\n",
        );
        assert_eq!(joined.to_plain(), "a\nb");
        assert_eq!(joined.spans().len(), 1, "empty parts contribute no separator");
    }

    #[test]
    fn cell_defaults_bold_every_span_without_touching_explicit_colour() {
        let red = Rgb::new(0xE6, 0x5B, 0x58);
        let text = StyledText::from_spans([
            TextSpan::plain("plain "),
            TextSpan::new("red", SpanStyle { color: Some(red), ..SpanStyle::default() }),
        ]);
        let header = text.with_cell_defaults(true, false, Some(Rgb::new(0x1A, 0x1D, 0x1F)));
        assert!(header.spans().iter().all(|s| s.style.bold));
        assert_eq!(header.spans()[1].style.color, Some(red));
        assert_eq!(header.spans()[0].style.color.map(|c| c.to_hex()).as_deref(), Some("#1a1d1f"));
    }

    /// The no-op path returns the text unchanged rather than rebuilding it, because
    /// layout calls this once per cell per frame on tables that have no headers.
    #[test]
    fn cell_defaults_with_nothing_to_apply_are_identity() {
        let text = StyledText::plain("unchanged");
        assert_eq!(text.with_cell_defaults(false, false, None), text);
    }

    #[test]
    fn char_len_counts_scalars_not_bytes() {
        let t = StyledText::plain("Kühlmittel");
        assert_eq!(t.char_len(), 10);
        assert_eq!(t.to_plain().len(), 11);
    }
}
