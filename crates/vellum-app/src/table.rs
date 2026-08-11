//! Tables on the board: the document token, and shaping-backed measurement.
//!
//! `vellum-table` owns the model — merges, spans, header rows, auto-fit column sizing,
//! resize and hit-testing — and lays a table out against a [`Measure`]. It has held all
//! of that, tested, since before the document could store one, and nothing could place
//! a table because [`vellum_doc::ItemKind`] had no slot for it.
//!
//! Two things live here.
//!
//! # The token
//!
//! Same shape as [`crate::shapes`], for the same reason: `vellum-doc` depends on `loro`
//! and `thiserror` and nothing else, and `docs/01-architecture.md` points the
//! dependency arrows strictly downward. So the document holds an opaque string and this
//! crate — which depends on both — owns the encoding.
//!
//! **This is a coarser grain than the rest of the document, and deliberately so.** A
//! sticky's text is a Loro rich-text container, so two edits to different words merge;
//! a table is one blob, so an edit replaces the whole thing. That costs nothing today —
//! collaboration is cut, and undo is per-gesture rather than per-character — and the
//! alternative is a Loro schema for merges, spans and per-cell styling before a single
//! table can be put on a board. When a table needs finer undo, the data is already here
//! and it is a re-encode rather than a re-import.
//!
//! # The measurer
//!
//! `vellum-table` cannot shape text — it is a layout crate with no font stack — so it
//! takes a [`Measure`]. Its own [`MonospaceMeasure`] says in its doc comment that it
//! *"should never reach the screen"*, and it is right: every glyph one ratio wide puts
//! column widths and row heights visibly wrong for a proportional face. [`ShapedMeasure`]
//! is the real one, backed by the same `vellum-text` engine the canvas draws with, so a
//! table's rows are as tall as the text that will actually be drawn in them.

use vellum_table::{
    Fit, Intrinsic, Measure, MeasureCache, Point as TablePoint, StyledText as TableText, Table,
    TableLayout, TextStyle,
};
use vellum_text::{LayoutParams, TextAlign, TextEngine};

/// The token stored in the document for a table.
pub fn encode(table: &Table) -> String {
    serde_json::to_string(table).unwrap_or_else(|error| {
        log::warn!("a table would not encode ({error}); storing an empty one");
        String::new()
    })
}

/// The table a token names.
///
/// **An unreadable token becomes a small empty table**, not an error and not a missing
/// item. A board written by a later build still opens, and the item keeps its size and
/// position — the same rule [`crate::shapes::decode`] follows.
pub fn decode(token: &str) -> Table {
    serde_json::from_str(token).unwrap_or_else(|error| {
        if !token.is_empty() {
            log::warn!("unreadable table ({error}); drawing an empty one");
        }
        Table::new(DEFAULT_ROWS, DEFAULT_COLUMNS)
    })
}

/// What the table tool places: Miro's own default, a 3 × 3 with a header row.
pub fn default_table() -> Table {
    let mut table = Table::new(DEFAULT_ROWS, DEFAULT_COLUMNS);
    // A header row is what makes a table read as a table rather than as a grid of
    // boxes, and it is what Miro's own insert does.
    table.set_header_rows(1);
    table
}

pub const DEFAULT_ROWS: usize = 3;
pub const DEFAULT_COLUMNS: usize = 3;

/// Every word in the table, in reading order, for search and export.
///
/// A table's text lives inside an opaque JSON token rather than in a
/// `vellum_doc::StyledText`, so `ItemKind::text` answers `None` for one and a search for a
/// word in a cell used to find nothing while the same word on a sticky was found. This is
/// the decoder search needs; see [`crate::words`].
///
/// Row by row, so the string reads the way the table does. Empty cells contribute nothing
/// rather than a run of separators — a mostly-empty table would otherwise index as
/// punctuation.
pub fn words(table: &Table) -> Vec<String> {
    // `anchors` is row-major and skips covered positions, so a merged cell answers once —
    // through its top-left — rather than once per coordinate it spans. It is also exactly
    // the set the painter lays out, which keeps "what is indexed" and "what is drawn" the
    // same list.
    table
        .grid()
        .anchors()
        .map(|(_, cell)| cell.content().to_plain())
        .filter(|text| !text.trim().is_empty())
        .collect()
}

/// A [`Measure`] backed by real shaping.
///
/// Borrows the engine rather than owning one: there is exactly one font stack in the
/// process, its atlas is on the GPU, and a second would double the resident font memory
/// to answer questions the first can already answer.
pub struct ShapedMeasure<'a> {
    engine: &'a mut TextEngine,
}

impl<'a> ShapedMeasure<'a> {
    pub fn new(engine: &'a mut TextEngine) -> Self {
        Self { engine }
    }

    /// A table cell's style as the text engine's parameters.
    ///
    /// `max_width` is set by the caller per question — `None` to ask how wide the text
    /// wants to be, `Some` to ask how tall it is at that width — which is exactly the
    /// two questions [`Measure`] asks.
    fn params(style: &TextStyle, max_width: Option<f32>) -> LayoutParams {
        LayoutParams {
            font_family: style.font_family.clone(),
            font_size: style.font_size as f32,
            line_height: style.line_height as f32,
            // Alignment moves glyphs inside a line and cannot change how much room the
            // line needs, so measuring always asks left-aligned. Feeding the cell's own
            // alignment in here would be a different question with the same answer.
            align: TextAlign::Left,
            max_width,
        }
    }
}

/// `vellum-table`'s spans as `vellum-text`'s.
///
/// Bold and italic ride on the span in one model and on the *style* in the other, so
/// the cell's own flags are folded into every span that does not already set them —
/// which is what `TextStyle`'s own documentation says they mean.
fn convert(text: &TableText, style: &TextStyle) -> vellum_text::StyledText {
    let spans: Vec<vellum_text::TextSpan> = text
        .spans()
        .iter()
        .map(|span| vellum_text::TextSpan {
            text: span.text.clone(),
            style: vellum_text::SpanStyle {
                bold: span.style.bold || style.bold,
                italic: span.style.italic || style.italic,
                underline: span.style.underline,
                strikethrough: span.style.strikethrough,
                link: span.style.link.clone(),
                // Colour changes no metric, and measuring is all this conversion is
                // for. The drawing path reads the span's own colour directly.
                color: None,
            },
        })
        .collect();
    vellum_text::StyledText::from_spans(spans)
}

/// A cell's words, for drawing.
///
/// The drawing path wants the span's own colour — unlike [`convert`], which is for
/// measuring and drops it because colour changes no metric.
pub fn to_text(content: &TableText) -> vellum_text::StyledText {
    let spans: Vec<vellum_text::TextSpan> = content
        .spans()
        .iter()
        .map(|span| vellum_text::TextSpan {
            text: span.text.clone(),
            style: vellum_text::SpanStyle {
                bold: span.style.bold,
                italic: span.style.italic,
                underline: span.style.underline,
                strikethrough: span.style.strikethrough,
                link: span.style.link.clone(),
                color: span.style.color.map(|c| vellum_text::Rgb { r: c.r, g: c.g, b: c.b }),
            },
        })
        .collect();
    vellum_text::StyledText::from_spans(spans)
}

impl Measure for ShapedMeasure<'_> {
    fn intrinsic(&mut self, content: &TableText, style: &TextStyle) -> Intrinsic {
        let text = convert(content, style);

        // Unwrapped: every paragraph on one line, which is the widest the cell would
        // ever want to be.
        let max = self.engine.measure(&text, &Self::params(style, None));

        // Unbreakable: the widest single word, which is the narrowest the column can be
        // without a glyph hanging outside it. Measured by shaping the longest word
        // alone rather than by wrapping at 1px, because a wrap that cannot fit still
        // reports the overflowing line's full width and the two would come out equal.
        let longest = content
            .to_plain()
            .split_whitespace()
            .max_by_key(|word| word.chars().count())
            .map(str::to_owned)
            .unwrap_or_default();
        let min = if longest.is_empty() {
            0.0
        } else {
            let word = vellum_text::StyledText::plain(&longest);
            f64::from(self.engine.measure(&word, &Self::params(style, None)).width)
        };

        Intrinsic { min_content: min, max_content: f64::from(max.width).max(min) }
    }

    fn height(&mut self, content: &TableText, style: &TextStyle, width: f64) -> f64 {
        let text = convert(content, style);
        #[expect(clippy::cast_possible_truncation, reason = "a column width is screen-scale")]
        let extent = self.engine.measure(&text, &Self::params(style, Some(width.max(1.0) as f32)));
        // At least one line. `Measure`'s contract is explicit that an empty cell is
        // still a cell and a row of them still has a height.
        let line = style.font_size * style.line_height;
        f64::from(extent.height).max(line)
    }
}

/// Lays a table out to fill `size`, with real shaping.
///
/// [`Fit::Width`] rather than the table's natural size: an item on a board has a box
/// the user dragged, and a table that ignored it would be the only kind that does.
pub fn layout(table: &Table, engine: &mut TextEngine, size: (f64, f64)) -> TableLayout {
    let mut measure = ShapedMeasure::new(engine);
    let mut cache = MeasureCache::default();
    table.layout(&mut measure, &mut cache, Fit::Width(size.0), TablePoint::new(0.0, 0.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine() -> TextEngine {
        TextEngine::new().expect("the test machine has fonts")
    }

    /// The round trip the document depends on.
    #[test]
    fn a_table_survives_the_trip_to_the_document_and_back() {
        let mut table = default_table();
        table.set_content(vellum_table::CellRef::new(0, 0), TableText::plain("Part")).unwrap();
        table
            .set_content(vellum_table::CellRef::new(1, 0), TableText::plain("Coolant hose"))
            .unwrap();

        let back = decode(&encode(&table));
        assert_eq!(back, table, "the table did not survive encoding");
        assert_eq!(back.header_rows(), 1);
    }

    /// A board from a later build still opens: the item keeps its size and position,
    /// and only the contents fall back.
    #[test]
    fn an_unreadable_token_becomes_an_empty_table_rather_than_failing() {
        let fallback = decode("{ not json");
        assert_eq!(fallback.rows().len(), DEFAULT_ROWS);
        assert_eq!(fallback.columns().len(), DEFAULT_COLUMNS);
        assert_eq!(decode("").rows().len(), DEFAULT_ROWS);
    }

    /// The reason this measurer exists. `MonospaceMeasure` gives every glyph the same
    /// advance, so "IIII" and "WWWW" come out identically wide; a real face does not.
    /// A column sized by the monospace stand-in is visibly wrong for the text that then
    /// gets drawn in it.
    #[test]
    fn shaping_distinguishes_narrow_text_from_wide_where_monospace_cannot() {
        let mut engine = engine();
        let mut measure = ShapedMeasure::new(&mut engine);
        let style = TextStyle::default();

        let narrow = measure.intrinsic(&TableText::plain("IIII"), &style);
        let wide = measure.intrinsic(&TableText::plain("WWWW"), &style);
        assert!(
            wide.max_content > narrow.max_content,
            "shaping reported {} vs {}",
            wide.max_content,
            narrow.max_content,
        );

        let mut mono = vellum_table::MonospaceMeasure::default();
        let m_narrow = mono.intrinsic(&TableText::plain("IIII"), &style);
        let m_wide = mono.intrinsic(&TableText::plain("WWWW"), &style);
        assert!(
            (m_wide.max_content - m_narrow.max_content).abs() < f64::EPSILON,
            "the stand-in was supposed to be unable to tell these apart",
        );
    }

    /// `Measure`'s contract: an empty cell is still a cell, and a row of them still has
    /// a height. Returning zero collapses the row to a line and the table to a smear.
    #[test]
    fn an_empty_cell_still_has_a_line_of_height() {
        let mut engine = engine();
        let mut measure = ShapedMeasure::new(&mut engine);
        let style = TextStyle::default();
        let height = measure.height(&TableText::default(), &style, 120.0);
        assert!(height >= style.font_size, "an empty cell reported {height}");
    }

    /// Wrapping makes a cell taller. If it did not, every row would be one line high
    /// and long text would spill out of the table.
    #[test]
    fn narrower_columns_make_taller_rows() {
        let mut engine = engine();
        let mut measure = ShapedMeasure::new(&mut engine);
        let style = TextStyle::default();
        let text = TableText::plain("the quick brown fox jumps over the lazy dog");

        let wide = measure.height(&text, &style, 400.0);
        let narrow = measure.height(&text, &style, 80.0);
        assert!(narrow > wide, "narrow {narrow} was not taller than wide {wide}");
    }

    /// The layout fills the box the user dragged, rather than the table's natural size.
    #[test]
    fn a_table_is_laid_out_to_the_width_it_was_given() {
        let mut engine = engine();
        let table = default_table();
        let out = layout(&table, &mut engine, (600.0, 300.0));

        assert!((out.size.width - 600.0).abs() < 1.0, "width {}", out.size.width);
        assert_eq!(out.column_offsets.len(), DEFAULT_COLUMNS + 1);
        assert_eq!(out.row_offsets.len(), DEFAULT_ROWS + 1);
        assert_eq!(out.cells.len(), DEFAULT_ROWS * DEFAULT_COLUMNS);
        // Boundaries are non-decreasing, which is what lets a coordinate be searched
        // into a column.
        assert!(out.column_offsets.windows(2).all(|w| w[1] >= w[0]));
        assert!(out.row_offsets.windows(2).all(|w| w[1] >= w[0]));
    }
}
