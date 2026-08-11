//! CSV export — sticky and card content, grouped by frame.
//!
//! # What Miro actually produces
//!
//! Measured against a real Miro CSV export — 205 lines from the reference board,
//! kept locally and git-ignored. Reading it rather than guessing settles the format:
//!
//! - A UTF-8 **BOM**, then LF line endings.
//! - **One section per frame.** The frame's name on its own line, quoted; then one
//!   row per item; then a blank line before the next section.
//! - Rows are `text` for a sticky or a text widget, and `title,url` for a link
//!   preview. A preview whose title never resolved is written as `url,url` — the
//!   file has several.
//! - Content is **not** un-escaped: `&#43;`, `&#39;` and `&amp;` appear verbatim,
//!   straight out of Miro's rich-text HTML. Vellum's text model already holds real
//!   characters (`vellum_text::html` decodes on import), so nothing here re-encodes
//!   them.
//! - Quoting is inconsistent. Sticky rows are always quoted; link-preview rows never
//!   are — so a preview whose title contains a comma produces a **malformed row**,
//!   and the reference export contains one (a Samsung tablet listing with six
//!   commas in its title). We do not reproduce that bug; see below.
//!
//! # What this writes
//!
//! Two dialects, because the two uses genuinely differ:
//!
//! - [`CsvDialect::Table`] — one header row, one row per item, with a **`frame`
//!   column**. This is what opens correctly in a spreadsheet and what a script can
//!   parse; it is the default.
//! - [`CsvDialect::MiroSections`] — Miro's section layout, for eyes already trained
//!   on it and for anything that consumes Miro's file.
//!
//! Both quote by RFC 4180 throughout: a field is quoted when it contains a comma, a
//! quote, a newline or leading/trailing space, and inner quotes are doubled.
//! Matching Miro's quoting exactly would mean matching its bug, and an export that
//! silently corrupts a row is worse than one that differs from Miro by a pair of
//! quotation marks.

use crate::error::ExportError;
use crate::item::Kind;
use crate::scene::Scene;

/// How the file is laid out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CsvDialect {
    /// `frame,type,text,link`, one header row, one row per item.
    #[default]
    Table,
    /// Miro's own shape: a quoted frame name, its rows, a blank line.
    MiroSections,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LineEnding {
    #[default]
    Lf,
    /// `\r\n`, which older Windows spreadsheet importers still prefer.
    Crlf,
}

impl LineEnding {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Lf => "\n",
            Self::Crlf => "\r\n",
        }
    }
}

#[derive(Debug, Clone)]
pub struct CsvOptions {
    pub dialect: CsvDialect,
    /// A UTF-8 byte-order mark. Excel on Windows reads a BOM-less UTF-8 CSV as the
    /// system code page and mangles every accented character, so Miro writes one and
    /// so do we.
    pub byte_order_mark: bool,
    pub line_ending: LineEnding,
    /// Which item kinds produce rows, by [`Kind::tag`]. The default is what Miro
    /// exports: the widgets that carry prose.
    pub kinds: Vec<&'static str>,
    /// Section name for items in no frame. `Table` writes it in the frame column.
    pub unframed: String,
    /// Replace newlines inside a cell with a space, as Miro does. Off — the
    /// default — keeps the paragraph breaks and relies on RFC 4180 quoting, which
    /// every spreadsheet handles.
    pub flatten_newlines: bool,
}

impl Default for CsvOptions {
    fn default() -> Self {
        Self {
            dialect: CsvDialect::Table,
            byte_order_mark: true,
            line_ending: LineEnding::Lf,
            kinds: vec!["sticky", "text", "card", "link_preview", "embed"],
            unframed: "(no frame)".to_string(),
            flatten_newlines: false,
        }
    }
}

impl CsvOptions {
    /// The closest reproduction of Miro's own file: sections, LF, BOM, and its
    /// newline flattening.
    pub fn miro() -> Self {
        Self {
            dialect: CsvDialect::MiroSections,
            flatten_newlines: true,
            ..Self::default()
        }
    }

    /// Adds shapes, which carry text on diagram-heavy boards but are absent from
    /// Miro's export.
    pub fn including_shapes(mut self) -> Self {
        if !self.kinds.contains(&"shape") {
            self.kinds.push("shape");
        }
        self
    }
}

/// One exported row, before it is formatted.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub frame: String,
    pub kind: &'static str,
    pub text: String,
    pub link: String,
}

/// The rows `scene` would produce, in export order: frames in presentation order,
/// items within a frame in z-order, unframed items last.
///
/// Exposed because it is also the useful thing for anything that wants the board's
/// prose without a file — search indexing, a word count, an AI summary.
pub fn rows(scene: &Scene, options: &CsvOptions) -> Vec<Row> {
    let mut rows = Vec::new();
    let wanted = |kind: &Kind| options.kinds.contains(&kind.tag());

    for frame in &scene.frames {
        let name = frame.title.clone().unwrap_or_else(|| format!("Frame {}", frame.order + 1));
        for placed in scene.items_in_frame(frame.id) {
            if wanted(&placed.item.kind)
                && let Some(row) = row_for(&placed.item, &name, options)
            {
                rows.push(row);
            }
        }
    }

    let framed: std::collections::BTreeSet<_> = scene.frames.iter().map(|f| f.id).collect();
    for placed in &scene.items {
        let in_known_frame = placed.item.frame.is_some_and(|f| framed.contains(&f));
        if !in_known_frame
            && wanted(&placed.item.kind)
            && let Some(row) = row_for(&placed.item, &options.unframed, options)
        {
            rows.push(row);
        }
    }
    rows
}

fn row_for(item: &crate::item::Item, frame: &str, options: &CsvOptions) -> Option<Row> {
    let mut text = item.plain_text();
    // A card with no body still has a title — a link preview's title is its name.
    if text.trim().is_empty() {
        text = item.name.clone().unwrap_or_default();
    }
    let link = item.link.clone().unwrap_or_default();
    // Exactly Miro's fallback: a preview whose title never resolved is written with
    // its URL in both columns rather than as a blank row.
    if text.trim().is_empty() {
        if link.is_empty() {
            return None;
        }
        text = link.clone();
    }
    if options.flatten_newlines {
        text = text.replace(['\n', '\r'], " ");
    }
    Some(Row { frame: frame.to_string(), kind: item.kind.tag(), text, link })
}

/// Writes `scene` as CSV.
pub fn write(scene: &Scene, options: &CsvOptions) -> Result<String, ExportError> {
    let rows = rows(scene, options);
    if rows.is_empty() {
        return Err(ExportError::NothingToExport);
    }
    let nl = options.line_ending.as_str();
    let mut out = String::with_capacity(rows.len() * 96);
    if options.byte_order_mark {
        out.push('\u{feff}');
    }

    match options.dialect {
        CsvDialect::Table => {
            out.push_str("frame,type,text,link");
            out.push_str(nl);
            for row in &rows {
                out.push_str(&field(&row.frame));
                out.push(',');
                out.push_str(row.kind);
                out.push(',');
                out.push_str(&field(&row.text));
                out.push(',');
                out.push_str(&field(&row.link));
                out.push_str(nl);
            }
        }
        CsvDialect::MiroSections => {
            let mut current: Option<&str> = None;
            for row in &rows {
                if current != Some(row.frame.as_str()) {
                    if current.is_some() {
                        out.push_str(nl);
                    }
                    // Miro quotes the section name unconditionally, and so do we:
                    // it is what marks a line as a heading rather than a row.
                    out.push('"');
                    out.push_str(&row.frame.replace('"', "\"\""));
                    out.push('"');
                    out.push_str(nl);
                    current = Some(&row.frame);
                }
                out.push_str(&field(&row.text));
                if !row.link.is_empty() {
                    out.push(',');
                    out.push_str(&field(&row.link));
                }
                out.push_str(nl);
            }
        }
    }
    Ok(out)
}

/// Writes CSV to a file.
pub fn write_file(
    scene: &Scene,
    options: &CsvOptions,
    path: impl AsRef<std::path::Path>,
) -> Result<(), ExportError> {
    let path = path.as_ref();
    let csv = write(scene, options)?;
    std::fs::write(path, csv).map_err(|e| ExportError::io(path.display(), e))
}

/// One field, quoted per RFC 4180 when it has to be.
///
/// Leading and trailing spaces force quoting too: they are not required to be
/// quoted, but several readers strip unquoted whitespace, and a sticky that says
/// `" TODO"` should not silently become `"TODO"`.
fn field(value: &str) -> String {
    let needs_quotes = value.contains([',', '"', '\n', '\r'])
        || value.starts_with(' ')
        || value.ends_with(' ');
    if needs_quotes {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geom::Rect;
    use crate::item::{Geometry, Item, ItemId, Kind};
    use crate::scene::Scene;
    use crate::source::{Scope, Snapshot};
    use crate::style::Color;
    use crate::text::{FontSpec, TextBlock};

    fn note(id: u64, kind: Kind, text: &str) -> Item {
        note_at(id, kind, text, Rect::new(0.0, 0.0, 10.0, 10.0))
    }

    fn note_at(id: u64, kind: Kind, text: &str, rect: Rect) -> Item {
        Item::new(id, kind, Geometry::rect(rect))
            .with_text(TextBlock::plain(text, FontSpec::default(), Color::BLACK))
    }

    /// Two frames as they sit on the reference board, plus an unframed sticky.
    fn scene() -> Scene {
        let items = vec![
            Item::new(1, Kind::frame(1), Geometry::rect(Rect::new(0.0, 0.0, 100.0, 100.0)))
                .with_name("Color"),
            Item::new(2, Kind::frame(0), Geometry::rect(Rect::new(200.0, 0.0, 100.0, 100.0)))
                .with_name("ECU"),
            note_at(3, Kind::Sticky, "Hockenheim Silver", Rect::new(10.0, 10.0, 30.0, 20.0))
                .in_frame(ItemId(1)),
            note_at(4, Kind::Sticky, "nardo grey", Rect::new(10.0, 40.0, 30.0, 20.0))
                .in_frame(ItemId(1)),
            note_at(5, Kind::LinkPreview, "KV16", Rect::new(210.0, 10.0, 30.0, 20.0))
                .in_frame(ItemId(2))
                .with_link("https://example.com/products/kv16"),
            note(6, Kind::Sticky, "loose"),
        ];
        Scene::collect(&Snapshot::new(items), &Scope::Board).expect("content")
    }

    #[test]
    fn rows_follow_presentation_order_then_unframed() {
        let rows = rows(&scene(), &CsvOptions::default());
        let frames: Vec<&str> = rows.iter().map(|r| r.frame.as_str()).collect();
        assert_eq!(frames, vec!["ECU", "Color", "Color", "(no frame)"]);
    }

    #[test]
    fn the_table_dialect_has_a_header_and_a_frame_column() {
        let csv = write(&scene(), &CsvOptions::default()).unwrap();
        let body = csv.trim_start_matches('\u{feff}');
        assert!(body.starts_with("frame,type,text,link\n"), "{body}");
        assert!(body.contains("ECU,link_preview,KV16,https://example.com/products/kv16\n"), "{body}");
        assert!(body.contains("Color,sticky,Hockenheim Silver,\n"), "{body}");
    }

    #[test]
    fn the_miro_dialect_writes_a_quoted_section_per_frame() {
        let csv = write(&scene(), &CsvOptions::miro()).unwrap();
        let body = csv.trim_start_matches('\u{feff}');
        // Exactly the layout of `Reference Board.csv`.
        assert!(body.starts_with("\"ECU\"\nKV16,https://example.com/products/kv16\n\n\"Color\"\n"), "{body}");
        assert!(body.contains("\n\n\"(no frame)\"\nloose\n"), "{body}");
    }

    #[test]
    fn a_byte_order_mark_is_written_by_default() {
        assert!(write(&scene(), &CsvOptions::default()).unwrap().starts_with('\u{feff}'));
        let bare = CsvOptions { byte_order_mark: false, ..CsvOptions::default() };
        assert!(!write(&scene(), &bare).unwrap().starts_with('\u{feff}'));
    }

    /// The row Miro corrupts: a preview title with commas in it.
    #[test]
    fn a_title_containing_commas_is_quoted_rather_than_split() {
        let title = "Samsung Galaxy Tab A9+ (64GB) 11'' Android Tablet, Big Screen, Quad Speakers";
        let items = vec![note(1, Kind::LinkPreview, title).with_link("https://example.com/x")];
        let scene = Scene::collect(&Snapshot::new(items), &Scope::Board).unwrap();
        let csv = write(&scene, &CsvOptions::miro()).unwrap();
        assert!(csv.contains(&format!("\"{title}\",https://example.com/x")), "{csv}");
    }

    #[test]
    fn inner_quotes_are_doubled() {
        assert_eq!(field("say \"hi\""), "\"say \"\"hi\"\"\"");
        assert_eq!(field("plain"), "plain");
        assert_eq!(field(" padded "), "\" padded \"");
        assert_eq!(field("two\nlines"), "\"two\nlines\"");
    }

    #[test]
    fn miro_flattens_newlines_and_the_table_dialect_keeps_them() {
        let items = vec![note(1, Kind::Sticky, "first\nsecond")];
        let scene = Scene::collect(&Snapshot::new(items), &Scope::Board).unwrap();
        assert!(write(&scene, &CsvOptions::miro()).unwrap().contains("first second"));
        assert!(write(&scene, &CsvOptions::default()).unwrap().contains("\"first\nsecond\""));
    }

    /// The `url,url` rows in the reference export.
    #[test]
    fn a_preview_with_no_title_falls_back_to_its_url_in_both_columns() {
        let url = "https://www.example.com/products/fuel-systems/parts/554-165";
        let items = vec![
            Item::new(1, Kind::LinkPreview, Geometry::rect(Rect::new(0.0, 0.0, 10.0, 10.0)))
                .with_link(url),
        ];
        let scene = Scene::collect(&Snapshot::new(items), &Scope::Board).unwrap();
        let rows = rows(&scene, &CsvOptions::default());
        assert_eq!(rows[0].text, url);
        assert_eq!(rows[0].link, url);
    }

    #[test]
    fn items_with_neither_text_nor_link_produce_no_row() {
        let items = vec![
            Item::new(1, Kind::Sticky, Geometry::rect(Rect::new(0.0, 0.0, 10.0, 10.0))),
            note(2, Kind::Sticky, "   "),
        ];
        let scene = Scene::collect(&Snapshot::new(items), &Scope::Board).unwrap();
        assert!(rows(&scene, &CsvOptions::default()).is_empty());
        assert!(matches!(write(&scene, &CsvOptions::default()), Err(ExportError::NothingToExport)));
    }

    #[test]
    fn ink_and_connectors_are_not_prose_and_are_left_out() {
        let default = CsvOptions::default();
        assert!(!default.kinds.contains(&"ink"));
        assert!(!default.kinds.contains(&"connector"));
        assert!(!default.kinds.contains(&"shape"));
        assert!(default.including_shapes().kinds.contains(&"shape"));
    }

    #[test]
    fn an_unnamed_frame_gets_a_numbered_section() {
        let items = vec![
            Item::new(1, Kind::frame(4), Geometry::rect(Rect::new(0.0, 0.0, 100.0, 100.0))),
            note(2, Kind::Sticky, "x").in_frame(ItemId(1)),
        ];
        let scene = Scene::collect(&Snapshot::new(items), &Scope::Board).unwrap();
        assert_eq!(rows(&scene, &CsvOptions::default())[0].frame, "Frame 5");
    }

    #[test]
    fn crlf_is_available_for_windows_spreadsheets() {
        let options = CsvOptions { line_ending: LineEnding::Crlf, ..CsvOptions::default() };
        let csv = write(&scene(), &options).unwrap();
        assert!(csv.contains("frame,type,text,link\r\n"), "{csv}");
        assert!(!csv.contains("link\n"), "{csv}");
    }
}
