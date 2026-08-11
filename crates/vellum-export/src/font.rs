//! Reading a font file well enough to embed it and to draw it.
//!
//! Two consumers, one module. The PDF writer needs glyph ids, advances and the
//! descriptor fields a `/FontDescriptor` requires; the CPU rasteriser needs glyph
//! *outlines*, because `tiny-skia` draws paths and knows nothing about text. Both
//! come out of the same `ttf-parser` face, so they live together.
//!
//! # This is not shaping
//!
//! Characters are mapped to glyphs one at a time through `cmap`, and advances come
//! straight from `hmtx`. There is no kerning, no ligature substitution, no mark
//! positioning and no bidi. Real shaping is `cosmic-text`'s job in `vellum-text`,
//! and `docs/01-architecture.md` §6 explains why it is not casually reproduced.
//!
//! What that costs, precisely: a PDF's glyphs are correct and selectable, and text
//! that the caller laid out is placed exactly where the caller said, but the
//! *within-line* advance used to centre or right-align a line is the unkerned sum.
//! For Latin text that is within a fraction of a percent; for Arabic or Devanagari
//! it is wrong, and the honest answer is that the caller should pass positioned
//! runs. The gap is recorded rather than hidden.

use crate::text::FontSpec;

/// A parsed font file.
pub(crate) struct Face<'a> {
    inner: ttf_parser::Face<'a>,
    units_per_em: f64,
}

/// One character mapped onto a glyph.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Glyph {
    pub id: u16,
    /// The character it came from — kept so a `/ToUnicode` map can be built and the
    /// PDF's text stays copyable.
    pub ch: char,
}

impl<'a> Face<'a> {
    pub(crate) fn parse(bytes: &'a [u8]) -> Option<Self> {
        let inner = ttf_parser::Face::parse(bytes, 0).ok()?;
        let units_per_em = f64::from(inner.units_per_em());
        (units_per_em > 0.0).then_some(Self { inner, units_per_em })
    }

    /// Glyphs for `text`, with `.notdef` (glyph 0) for anything the font has no
    /// mapping for. Substituting rather than dropping keeps the visible tofu that
    /// tells a user a character is missing, instead of silently losing it.
    pub(crate) fn glyphs(&self, text: &str) -> Vec<Glyph> {
        text.chars()
            .map(|ch| Glyph { id: self.inner.glyph_index(ch).map_or(0, |g| g.0), ch })
            .collect()
    }

    /// Advance in em units, where 1.0 is the font size.
    pub(crate) fn advance(&self, glyph: u16) -> f64 {
        self.inner
            .glyph_hor_advance(ttf_parser::GlyphId(glyph))
            .map_or(0.0, |a| f64::from(a) / self.units_per_em)
    }

    pub(crate) fn text_width(&self, text: &str, size: f64) -> f64 {
        self.glyphs(text).iter().map(|g| self.advance(g.id)).sum::<f64>() * size
    }

    /// Metrics a PDF `/FontDescriptor` requires, all in em units.
    pub(crate) fn descriptor(&self) -> Descriptor {
        let bbox = self.inner.global_bounding_box();
        let em = self.units_per_em;
        Descriptor {
            postscript_name: self.postscript_name(),
            ascent: f64::from(self.inner.ascender()) / em,
            descent: f64::from(self.inner.descender()) / em,
            cap_height: self
                .inner
                .capital_height()
                .map_or(0.7, |v| f64::from(v) / em),
            italic_angle: f64::from(self.inner.italic_angle()),
            bbox: [
                f64::from(bbox.x_min) / em,
                f64::from(bbox.y_min) / em,
                f64::from(bbox.x_max) / em,
                f64::from(bbox.y_max) / em,
            ],
            is_italic: self.inner.is_italic(),
            is_bold: self.inner.is_bold(),
            glyph_count: self.inner.number_of_glyphs(),
        }
    }

    /// The font's own PostScript name, or a sanitised fallback.
    ///
    /// PDF `/BaseFont` names may not contain spaces or delimiters, so anything
    /// outside the safe set is dropped rather than escaped — a viewer matches on
    /// the embedded file, not on this string.
    fn postscript_name(&self) -> String {
        let raw = self
            .inner
            .names()
            .into_iter()
            .find(|n| n.name_id == ttf_parser::name_id::POST_SCRIPT_NAME)
            .and_then(|n| n.to_string())
            .unwrap_or_default();
        let cleaned: String = raw
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '+' | '.' | '_'))
            .collect();
        if cleaned.is_empty() { "EmbeddedFont".to_string() } else { cleaned }
    }

    /// A glyph's outline in em units, y **downwards** to match board space.
    ///
    /// Font outlines are y-up by definition, so this negates y once here rather
    /// than leaving every caller to remember. Returns `None` for glyphs with no
    /// contours, which is the normal answer for a space.
    pub(crate) fn outline(&self, glyph: u16) -> Option<crate::geom::Path> {
        let mut builder = PathCollector::new(self.units_per_em);
        self.inner.outline_glyph(ttf_parser::GlyphId(glyph), &mut builder)?;
        builder.finish()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Descriptor {
    pub postscript_name: String,
    pub ascent: f64,
    pub descent: f64,
    pub cap_height: f64,
    pub italic_angle: f64,
    pub bbox: [f64; 4],
    pub is_italic: bool,
    pub is_bold: bool,
    pub glyph_count: u16,
}

/// Turns `ttf-parser`'s outline callbacks into a [`crate::geom::Path`].
struct PathCollector {
    scale: f64,
    subpaths: Vec<crate::geom::SubPath>,
    start: crate::geom::Point,
    segments: Vec<crate::geom::Segment>,
    open: bool,
}

impl PathCollector {
    fn new(units_per_em: f64) -> Self {
        Self {
            scale: 1.0 / units_per_em,
            subpaths: Vec::new(),
            start: crate::geom::Point::default(),
            segments: Vec::new(),
            open: false,
        }
    }

    fn point(&self, x: f32, y: f32) -> crate::geom::Point {
        crate::geom::pt(f64::from(x) * self.scale, -f64::from(y) * self.scale)
    }

    fn flush(&mut self) {
        if self.open && !self.segments.is_empty() {
            self.subpaths.push(crate::geom::SubPath::closed(
                self.start,
                std::mem::take(&mut self.segments),
            ));
        }
        self.segments.clear();
        self.open = false;
    }

    fn finish(mut self) -> Option<crate::geom::Path> {
        self.flush();
        (!self.subpaths.is_empty()).then(|| crate::geom::Path::new(self.subpaths))
    }
}

impl ttf_parser::OutlineBuilder for PathCollector {
    fn move_to(&mut self, x: f32, y: f32) {
        self.flush();
        self.start = self.point(x, y);
        self.open = true;
    }

    fn line_to(&mut self, x: f32, y: f32) {
        let to = self.point(x, y);
        self.segments.push(crate::geom::Segment::Line { to });
    }

    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        let ctrl = self.point(x1, y1);
        let to = self.point(x, y);
        self.segments.push(crate::geom::Segment::Quadratic { ctrl, to });
    }

    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        let ctrl1 = self.point(x1, y1);
        let ctrl2 = self.point(x2, y2);
        let to = self.point(x, y);
        self.segments.push(crate::geom::Segment::Cubic { ctrl1, ctrl2, to });
    }

    fn close(&mut self) {
        self.flush();
    }
}

/// Adobe's Helvetica advance widths for printable ASCII, in 1/1000 em.
///
/// PDF's fourteen standard fonts need no embedded file, and Helvetica is the
/// fallback when a caller supplies no font bytes. Its widths are a fixed part of
/// the PDF specification, so this table is a constant rather than a guess — without
/// it, centred and right-aligned text in a fallback PDF would be misplaced by
/// whatever the average character width happened to be.
const HELVETICA_WIDTHS: [u16; 95] = [
    278, 278, 355, 556, 556, 889, 667, 191, 333, 333, 389, 584, 278, 333, 278, 278, // ' '..'/'
    556, 556, 556, 556, 556, 556, 556, 556, 556, 556, 278, 278, 584, 584, 584, 556, // '0'..'?'
    1015, 667, 667, 722, 722, 667, 611, 778, 722, 278, 500, 667, 556, 833, 722, 778, // '@'..'O'
    667, 778, 722, 667, 611, 722, 667, 944, 667, 667, 611, 278, 278, 278, 469, 556, // 'P'.._
    333, 556, 556, 500, 556, 556, 278, 556, 556, 222, 222, 500, 222, 833, 556, 556, // '`'..'o'
    556, 556, 333, 500, 278, 556, 500, 722, 500, 500, 500, 334, 260, 334, 584, // 'p'..'~'
];

/// One character's advance in Helvetica, in em units. Characters outside printable
/// ASCII fall back to the width of a lowercase `n`, which is the closest thing to
/// an average this table has.
pub(crate) fn helvetica_advance(ch: char) -> f64 {
    let index = (ch as u32).checked_sub(0x20).and_then(|i| usize::try_from(i).ok());
    let thousandths = index.and_then(|i| HELVETICA_WIDTHS.get(i)).copied().unwrap_or(556);
    f64::from(thousandths) / 1000.0
}

pub(crate) fn helvetica_width(text: &str, size: f64) -> f64 {
    text.chars().map(helvetica_advance).sum::<f64>() * size
}

/// The width of `text`, using the embedded face when there is one and Helvetica's
/// metrics when there is not.
pub(crate) fn measure(font_bytes: Option<&[u8]>, text: &str, spec: &FontSpec) -> f64 {
    match font_bytes.and_then(Face::parse) {
        Some(face) => face.text_width(text, spec.size),
        None => helvetica_width(text, spec.size),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helvetica_widths_match_the_published_metrics() {
        // Spot checks against Adobe's Helvetica.afm.
        assert_eq!(helvetica_advance(' '), 0.278);
        assert_eq!(helvetica_advance('A'), 0.667);
        assert_eq!(helvetica_advance('i'), 0.222);
        assert_eq!(helvetica_advance('W'), 0.944);
        assert_eq!(helvetica_advance('~'), 0.584);
        assert_eq!(HELVETICA_WIDTHS.len(), 95, "printable ASCII is 0x20..=0x7e");
    }

    #[test]
    fn characters_outside_the_table_get_an_average_rather_than_zero() {
        assert_eq!(helvetica_advance('é'), 0.556);
        assert_eq!(helvetica_advance('\u{1}'), 0.556);
        assert!(helvetica_width("", 12.0) == 0.0);
    }

    #[test]
    fn helvetica_widths_scale_with_size() {
        let at_ten = helvetica_width("Hello", 10.0);
        let at_twenty = helvetica_width("Hello", 20.0);
        assert!((at_twenty - at_ten * 2.0).abs() < 1e-9);
        // "Hello" = H 722 + e 556 + l 222 + l 222 + o 556 = 2278/1000 em.
        assert!((at_ten - 22.78).abs() < 1e-9, "{at_ten}");
    }

    #[test]
    fn a_face_that_is_not_a_font_is_rejected_rather_than_panicking() {
        assert!(Face::parse(b"not a font at all").is_none());
        assert!(Face::parse(&[]).is_none());
    }

    #[test]
    fn measuring_falls_back_to_helvetica_without_a_face() {
        let spec = FontSpec::new("Inter", 10.0);
        assert_eq!(measure(None, "Hello", &spec), helvetica_width("Hello", 10.0));
        assert_eq!(measure(Some(b"junk"), "Hello", &spec), helvetica_width("Hello", 10.0));
    }
}
