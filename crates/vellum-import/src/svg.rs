//! Reads Miro's SVG export.
//!
//! Miro's vector export is not flattened artwork: it retains Miro's own CSS class
//! names and symbol ids, so widget *types* survive. That makes it an **independent
//! oracle** for the clipboard importer — the two formats are produced by different
//! Miro code paths, so agreement between them is strong evidence an import is
//! complete, and disagreement localises the bug.
//!
//! Counting is done by streaming rather than building a DOM: the reference board's
//! export is 36MB, and this runs on every import to produce the fidelity report.
//!
//! The markers below were established by analysing that export; see
//! `docs/02-miro-formats.md` §3 for the evidence and the expected counts.

use anyhow::{Context, Result};
use quick_xml::events::Event;
use std::collections::BTreeMap;
use std::io::BufRead;

/// A stroke's `d` attribute longer than this is ink or complex geometry rather
/// than a shape outline or an arrowhead. Chosen from the reference board, where it
/// cleanly separates 134 ink paths from ~96 short decorative paths.
const INK_PATH_MIN_LEN: usize = 500;

/// What a Miro SVG export contains, by widget type.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SvgInventory {
    /// Sticky notes, from `<use xlink:href="#StickerType1|2">` instances.
    pub stickies: usize,
    /// Sticky fill colours, e.g. `"#fff79e" -> 43`. The strongest single
    /// cross-check available, since colour survives both formats losslessly.
    pub sticky_colors: BTreeMap<String, usize>,
    /// `data-frame="true"`.
    pub frames: usize,
    /// `class="shape-element …"` — Miro's generic rect primitive, **not** a shape
    /// widget. On the reference board all 47 are widget chrome: 46 are the
    /// transparent background of a `text` widget (fill and stroke both
    /// `transparent`) and the 47th is the rich document's white page. Miro's REST
    /// API reports **zero** `shape` items for that same board, so a count here is
    /// not evidence of a shape waiting to be imported.
    pub shape_element_rects: usize,
    /// `class="preview-widget …"` — link preview cards.
    pub link_previews: usize,
    /// `class="embed-widget"`.
    pub embeds: usize,
    /// Connector arrowheads (`#LineHeadArrow…`). Note a connector may have two, so
    /// this is an upper bound on connectors, not an exact count.
    pub connector_arrowheads: usize,
    /// `<path>` elements long enough to be ink.
    pub ink_paths: usize,
    /// `<text>` elements with non-whitespace content.
    pub text_strings: usize,
    /// `data:image/*;base64` payloads.
    pub embedded_images: usize,
}

/// Board extent in px, from the root `<svg width height>`.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Extent {
    pub width: f64,
    pub height: f64,
}

/// The result of reading an export.
#[derive(Debug, Clone, Default)]
pub struct SvgExport {
    pub extent: Extent,
    pub inventory: SvgInventory,
}

/// Streams a Miro SVG export and counts what it contains.
pub fn read(reader: impl BufRead) -> Result<SvgExport> {
    let mut xml = quick_xml::Reader::from_reader(reader);
    xml.config_mut().check_end_names = false; // exports embed raw HTML in <foreignObject>

    let mut out = SvgExport::default();
    let mut buf = Vec::new();
    // `<text>` content arrives as separate text events; only count elements that
    // actually carry visible characters.
    let mut in_text = false;
    let mut text_has_content = false;

    loop {
        match xml.read_event_into(&mut buf) {
            Err(e) => return Err(e).context("malformed SVG"),
            Ok(Event::Eof) => break,
            Ok(Event::Start(e) | Event::Empty(e)) => {
                let name = e.local_name();
                let tag = String::from_utf8_lossy(name.as_ref()).to_string();

                // Attributes are read once into a small map; these elements have
                // few attributes, so this stays cheap across millions of events.
                let mut attrs: BTreeMap<String, String> = BTreeMap::new();
                for a in e.attributes().flatten() {
                    let k = String::from_utf8_lossy(a.key.as_ref()).to_string();
                    let v = String::from_utf8_lossy(&a.value).to_string();
                    attrs.insert(k, v);
                }

                if tag == "svg" && out.extent.width == 0.0 {
                    out.extent.width = px(attrs.get("width"));
                    out.extent.height = px(attrs.get("height"));
                }

                if let Some(class) = attrs.get("class") {
                    // `shape-element-rect` etc. all start with `shape-element`.
                    // Counted for the record, but this is Miro's rect primitive and
                    // is emitted as the *background* of text and document widgets —
                    // it does not imply a shape widget. See the field's doc comment.
                    if class.starts_with("shape-element") {
                        out.inventory.shape_element_rects += 1;
                    }
                    // Trailing space distinguishes the outer `preview-widget X`
                    // container from inner `preview-content_*` parts.
                    if class.starts_with("preview-widget ") {
                        out.inventory.link_previews += 1;
                    }
                    if class == "embed-widget" {
                        out.inventory.embeds += 1;
                    }
                }

                if attrs.get("data-frame").map(String::as_str) == Some("true") {
                    out.inventory.frames += 1;
                }

                // `xlink:href` and `href` both appear depending on the element.
                let href = attrs.get("xlink:href").or_else(|| attrs.get("href"));
                if let Some(h) = href {
                    // Exact match only: `#StickerType1Path` is the shared geometry
                    // definition inside <defs>, not a sticky instance.
                    if h == "#StickerType1" || h == "#StickerType2" {
                        out.inventory.stickies += 1;
                        if let Some(fill) = attrs.get("fill") {
                            *out.inventory.sticky_colors.entry(fill.to_lowercase()).or_default() += 1;
                        }
                    }
                    if h.starts_with("#LineHeadArrow") {
                        out.inventory.connector_arrowheads += 1;
                    }
                    if h.starts_with("data:image/") && h.contains(";base64,") {
                        out.inventory.embedded_images += 1;
                    }
                }

                if tag == "path"
                    && attrs.get("d").is_some_and(|d| d.len() >= INK_PATH_MIN_LEN)
                {
                    out.inventory.ink_paths += 1;
                }

                if tag == "text" {
                    in_text = true;
                    text_has_content = false;
                }
            }
            Ok(Event::Text(t)) if in_text => {
                if !t.into_inner().iter().all(|b| b.is_ascii_whitespace()) {
                    text_has_content = true;
                }
            }
            Ok(Event::End(e)) if e.local_name().as_ref() == b"text" => {
                if in_text && text_has_content {
                    out.inventory.text_strings += 1;
                }
                in_text = false;
            }
            _ => {}
        }
        buf.clear();
    }
    Ok(out)
}

/// Reads an export from a path.
pub fn read_file(path: impl AsRef<std::path::Path>) -> Result<SvgExport> {
    let path = path.as_ref();
    let file = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    // 36MB of XML: a generous buffer measurably beats the default.
    read(std::io::BufReader::with_capacity(1 << 20, file))
        .with_context(|| format!("reading {}", path.display()))
}

/// Parses `"41282.89px"` into `41282.89`.
fn px(v: Option<&String>) -> f64 {
    v.map(|s| s.trim_end_matches("px").trim().parse().unwrap_or(0.0)).unwrap_or(0.0)
}

/// One disagreement between the SVG oracle and an import.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Discrepancy {
    pub what: String,
    pub svg_says: usize,
    pub import_says: usize,
}

impl std::fmt::Display for Discrepancy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: SVG has {}, import produced {}", self.what, self.svg_says, self.import_says)
    }
}

impl SvgInventory {
    /// Compares this inventory against per-type counts from an import.
    ///
    /// Only types both formats represent are compared. An empty result means the
    /// import is consistent with the oracle; anything returned is a decoder bug
    /// worth chasing.
    pub fn compare(&self, import_counts: &BTreeMap<String, usize>) -> Vec<Discrepancy> {
        let get = |k: &str| import_counts.get(k).copied().unwrap_or(0);
        let mut out: Vec<Discrepancy> = [
            ("sticky", self.stickies, get("sticky")),
            ("frame", self.frames, get("frame")),
            ("link_preview", self.link_previews, get("link_preview")),
            ("embed", self.embeds, get("embed")),
            ("connector", self.connector_arrowheads, get("connector")),
            ("image", self.embedded_images, get("image")),
        ]
        .into_iter()
        .filter(|(_, a, b)| a != b)
        .map(|(what, svg_says, import_says)| Discrepancy {
            what: what.to_string(),
            svg_says,
            import_says,
        })
        .collect();

        // Ink is a *floor*, not an equality: the SVG count only includes paths
        // over INK_PATH_MIN_LEN, so short strokes are legitimately absent from it.
        // An import having more ink than the SVG is expected; having less is a bug.
        let imported_ink = get("ink");
        if imported_ink < self.ink_paths {
            out.push(Discrepancy {
                what: "ink (import has fewer than the SVG's long strokes)".to_string(),
                svg_says: self.ink_paths,
                import_says: imported_ink,
            });
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mirrors the real export's structure, including the traps: the `<defs>`
    /// `#StickerType1Path` that must NOT count as a sticky, `preview-content_*`
    /// inner elements that must not count as previews, and a short decorative
    /// path that must not count as ink.
    const SAMPLE: &str = r##"<?xml version="1.0"?>
<svg width="41282.89px" height="17515.36px">
  <defs>
    <g id="StickerType1"><use fill="inherit" xlink:href="#StickerType1Path"/></g>
    <g id="StickerType2"><use fill="inherit" xlink:href="#StickerType2Path"/></g>
  </defs>
  <use xlink:href="#StickerType1" transform="translate(1,2)" fill="#FFF79E"/>
  <use xlink:href="#StickerType1" transform="translate(3,4)" fill="#fff79e"/>
  <use xlink:href="#StickerType2" transform="translate(5,6)" fill="#ff9e9e"/>
  <g data-frame="true"><rect/></g>
  <rect class="shape-element shape-element-rect"/>
  <rect class="shape-background"/>
  <foreignObject><div class="preview-widget preview-widget_bigimage">
    <div class="preview-content"><span class="preview-content_title">t</span></div>
  </div></foreignObject>
  <foreignObject><div class="embed-widget"><span class="embed-content_title">e</span></div></foreignObject>
  <use xlink:href="#LineHeadArrow2"/>
  <path d="M0,0 L1,1"/>
  <path d="__LONG__"/>
  <text x="0" y="14">Manifold Absolute Pressure (MAP) sensor</text>
  <text x="0" y="30">   </text>
  <image href="data:image/jpeg;base64,AAAA"/>
</svg>"##;

    fn parse() -> SvgExport {
        let long = "M0,0 ".repeat(200); // comfortably over INK_PATH_MIN_LEN
        let doc = SAMPLE.replace("__LONG__", &long);
        read(doc.as_bytes()).unwrap()
    }

    #[test]
    fn reads_board_extent() {
        let e = parse().extent;
        assert_eq!(e.width, 41282.89);
        assert_eq!(e.height, 17515.36);
    }

    /// The `<defs>` definitions reference `#StickerType1Path`, not `#StickerType1`,
    /// so only real instances are counted.
    #[test]
    fn counts_sticky_instances_not_definitions() {
        let inv = parse().inventory;
        assert_eq!(inv.stickies, 3);
        assert_eq!(inv.sticky_colors.get("#fff79e"), Some(&2));
        assert_eq!(inv.sticky_colors.get("#ff9e9e"), Some(&1));
    }

    #[test]
    fn counts_widget_types_without_double_counting_inner_elements() {
        let inv = parse().inventory;
        assert_eq!(inv.frames, 1);
        assert_eq!(
            inv.shape_element_rects, 1,
            "the `shape-background` wrapper is a different class and must not be counted too"
        );
        assert_eq!(inv.link_previews, 1, "preview-content_* must not count");
        assert_eq!(inv.embeds, 1);
        assert_eq!(inv.connector_arrowheads, 1);
        assert_eq!(inv.embedded_images, 1);
    }

    #[test]
    fn separates_ink_from_short_decorative_paths() {
        assert_eq!(parse().inventory.ink_paths, 1);
    }

    /// Whitespace-only `<text>` is layout padding, not content.
    #[test]
    fn counts_only_text_with_content() {
        assert_eq!(parse().inventory.text_strings, 1);
    }

    #[test]
    fn compare_is_silent_when_import_matches() {
        let inv = SvgInventory { stickies: 44, ink_paths: 134, ..Default::default() };
        let counts = BTreeMap::from([("sticky".to_string(), 44), ("ink".to_string(), 134)]);
        assert!(inv.compare(&counts).is_empty());
    }

    #[test]
    fn compare_reports_a_shortfall() {
        let inv = SvgInventory { stickies: 44, ..Default::default() };
        let counts = BTreeMap::from([("sticky".to_string(), 40)]);
        let d = inv.compare(&counts);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].svg_says, 44);
        assert_eq!(d[0].import_says, 40);
        assert!(d[0].to_string().contains("SVG has 44"), "{}", d[0]);
    }

    #[test]
    fn malformed_xml_errors() {
        assert!(read(b"<svg><unclosed".as_slice()).is_err());
    }
}
