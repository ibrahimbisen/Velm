//! Miro's widget model, normalised into something Vellum can build a board from.
//!
//! Field names and the compact style keys were recovered by decoding real clipboard
//! payloads; Miro publishes no schema for any of this. Mappings are annotated with
//! how confident we are, because a guess that silently renders the wrong colour is
//! worse than an honest gap. Anything unrecognised is preserved verbatim in
//! [`Widget::raw`] and counted in the [`FidelityReport`], so an import never
//! quietly drops content.

use serde::{Deserialize, Serialize};

/// A widget's placement on the board, in Vellum's f64 world space.
///
/// **`x`/`y` are the widget's centre**, matching Miro's own convention.
///
/// Miro emits two coordinate schemas and the distinction is not cosmetic: on the
/// reference board **485 of 596 widgets (81%)** use `parentOffsetPx`, measured from
/// their parent frame's **top-left corner**, not the canvas origin. Establishing
/// that empirically mattered — resolving those offsets against the parent's centre
/// instead places only 18.6% of widgets inside their own frame, against 94.4% for
/// top-left. The mapper resolves everything to absolute before constructing this,
/// so consumers never deal with a mixed coordinate space.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Placement {
    /// Absolute world-space centre.
    pub x: f64,
    pub y: f64,
    /// Uniform scale. Miro also carries `relativeScale`, which differs only for
    /// widgets inside a scaled parent; we resolve against absolute scale.
    pub scale: f64,
    /// Rotation in degrees, clockwise.
    pub rotation: f64,
    pub width: Option<f64>,
    pub height: Option<f64>,
}

impl Default for Placement {
    fn default() -> Self {
        Self { x: 0.0, y: 0.0, scale: 1.0, rotation: 0.0, width: None, height: None }
    }
}

/// An sRGB colour. Miro encodes colours as a decimal integer, with `-1` meaning
/// "none" — verified against the SVG export, where sticky `sbc: 16775070` renders
/// as `#fff79e`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl Rgb {
    /// Decodes Miro's integer colour form. `-1` (and any out-of-range value) is
    /// "no colour" rather than a black fill.
    pub fn from_miro(v: i64) -> Option<Self> {
        if !(0..=0xFF_FFFF).contains(&v) {
            return None;
        }
        Some(Self((v >> 16) as u8, (v >> 8) as u8, v as u8))
    }

    pub fn to_hex(self) -> String {
        format!("#{:02x}{:02x}{:02x}", self.0, self.1, self.2)
    }
}

/// Horizontal text alignment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Align {
    Left,
    Center,
    Right,
}

/// Text styling shared by stickies and text widgets.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TextStyle {
    /// `ffn` — font family name, e.g. `"Noto Sans"`.
    pub font_family: Option<String>,
    /// `fs` — font size in px. `0` paired with `fsa: 1` means auto-fit, which we
    /// represent as `None` so the layout engine sizes it.
    pub font_size: Option<f64>,
    /// `tc` — text colour.
    pub color: Option<Rgb>,
    /// `ta` — horizontal alignment.
    pub align: Option<Align>,
    /// `lh` — line height as a multiple of font size.
    pub line_height: Option<f64>,
    /// `b` / `i` / `u` / `s` — 0/1 flags.
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strikethrough: bool,
}

/// One stroke of freehand ink. Miro calls these `paint` widgets, and they are the
/// single most valuable thing the clipboard gives us: no Miro REST or Web SDK
/// endpoint exposes drawings at all.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Ink {
    /// Stroke points, relative to the widget's placement.
    pub points: Vec<(f64, f64)>,
    /// `lc` — stroke colour.
    pub color: Option<Rgb>,
    /// `t` — stroke thickness in px.
    pub thickness: Option<f64>,
    /// `lo` — stroke opacity, 0.0–1.0.
    pub opacity: Option<f64>,
}

/// A reference to a binary asset. The `id` is the join key to a `.rtb` archive,
/// where assets are stored as `<id>.<ext>` — the highest-quality pixel source we
/// have, since the clipboard carries references only.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssetRef {
    pub id: String,
    pub name: Option<String>,
    pub width: Option<f64>,
    pub height: Option<f64>,
    /// Set for documents/images that originated from a URL.
    pub external_url: Option<String>,
}

/// What a widget actually is. Unknown Miro types land in [`WidgetKind::Unsupported`]
/// rather than being dropped, so the fidelity report can name them exactly.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum WidgetKind {
    /// `sticker` — a sticky note. `text` is rich-text HTML such as `<p>fan</p>`.
    Sticky { html: String, background: Option<Rgb>, style: TextStyle },
    /// `text` — a standalone text widget, also rich-text HTML.
    Text { html: String, style: TextStyle },
    /// `paint` — freehand ink.
    Ink(Ink),
    /// `image` — a bitmap, possibly cropped.
    Image { asset: AssetRef, crop: Option<Crop> },
    /// `document` — an embedded PDF.
    Document { asset: AssetRef },
    /// `embed` — a rich link/oEmbed card.
    ///
    /// Everything past `description` is metadata **Miro has already fetched** and
    /// stores in the payload, and taking it is what makes an imported card look like
    /// the one on the board. It matters more than it sounds: measured against the
    /// reference board's own links, re-fetching is not a substitute — Amazon answers
    /// **404** and eBay **403** to a non-browser agent, and Alibaba serves a page with
    /// no OpenGraph tags at all. That is 49 of 131 links that can never be filled in
    /// from the live web, and all of them arrive complete in the clipboard.
    Embed {
        title: Option<String>,
        url: Option<String>,
        description: Option<String>,
        /// `provider.name` / `custom_data.provider_name` — "YouTube", "Alibaba".
        provider: Option<String>,
        /// `custom_data.html` — the provider's `<iframe>` markup, verbatim. Not
        /// rendered (see `vellum_doc::ItemKind::Embed`), kept so the no-webview
        /// decision stays reversible without a re-import.
        html: Option<String>,
        /// `resourceWidget` — the preview image, which Miro downloaded and stored as
        /// a board resource. Joins to the `.rtb` like any other asset, and its
        /// `meta.externalLink` is a direct CDN URL when the archive lacks it.
        image: Option<AssetRef>,
    },
    /// `frame` — a named region that contains other widgets and doubles as a slide.
    /// `order` is Miro's `prevFrameIndex`, which fixes presentation sequence.
    Frame {
        title: String,
        background: Option<Rgb>,
        order: Option<i64>,
        speaker_notes: Option<String>,
    },
    /// `line` — a connector. Endpoints bind to widgets, so they must re-route when
    /// either end moves; that binding is the whole value of importing them.
    Connector {
        start: ConnectorEnd,
        end: ConnectorEnd,
        style: ConnectorStyle,
        /// Text labels riding on the connector.
        captions: Vec<String>,
    },
    /// `preview` — a link card built from the target's OpenGraph metadata.
    ///
    /// See [`WidgetKind::Embed`] for why the fetched-already fields are taken rather
    /// than re-fetched.
    LinkPreview {
        title: Option<String>,
        url: Option<String>,
        description: Option<String>,
        /// `resourceWidget` — Miro's stored copy of the preview image. Present on 64
        /// of the reference board's 91 previews, and **57 of those are in the `.rtb`**,
        /// so most cards get their picture with no network at all.
        image: Option<AssetRef>,
        /// `visualType` — which of Miro's card forms the user chose for *this* card.
        /// Kept raw rather than mapped here so the mapping lives with the document's
        /// own `CardMode`; see `pipeline::card_mode`.
        visual_type: Option<i64>,
    },
    /// `structured_document` — rich text held as Quill delta ops.
    RichDocument { ops: Vec<DeltaOp> },
    /// A group. Arrives as a distinct object shape (`type: 10`) rather than a
    /// widget, referencing its children by index.
    Group { children: Vec<usize> },
    /// A Miro type we do not map yet. The raw JSON is kept on the widget.
    Unsupported { miro_type: String },
}

/// One end of a connector.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConnectorEnd {
    /// The widget this end binds to, as a Miro object index. `None` for an
    /// endpoint floating free on the canvas.
    pub target: Option<usize>,
    /// Attachment point on the target, normalised 0–1 across its bounds.
    /// `{x: 1, y: 0.5}` is the right edge, vertically centred.
    pub anchor: (f64, f64),
    /// Miro's arrowhead code (`a_start` / `a_end`); 0 is none.
    pub arrowhead: i64,
}

/// Connector line styling, from the compact style keys.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ConnectorStyle {
    /// `lc` — line colour.
    pub color: Option<Rgb>,
    /// `t` — thickness in px.
    pub thickness: Option<f64>,
    /// `ls` — dash pattern code.
    pub dash: Option<i64>,
    /// `lt` — routing: straight, elbow or curved.
    pub routing: Option<i64>,
    /// `jump` — whether crossings hop over one another.
    pub jump_overs: bool,
}

/// A Quill delta operation — the format Miro uses for `structured_document`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeltaOp {
    /// The inserted text.
    pub insert: String,
    /// Attributes such as `color`, `header`, `bold`. Kept as raw JSON because the
    /// full attribute vocabulary is not yet established.
    pub attributes: Option<serde_json::Value>,
}

impl WidgetKind {
    /// Stable label used in fidelity reports and logs.
    pub fn label(&self) -> &str {
        match self {
            Self::Sticky { .. } => "sticky",
            Self::Text { .. } => "text",
            Self::Ink(_) => "ink",
            Self::Image { .. } => "image",
            Self::Document { .. } => "document",
            Self::Embed { .. } => "embed",
            Self::Frame { .. } => "frame",
            Self::Connector { .. } => "connector",
            Self::LinkPreview { .. } => "link_preview",
            Self::RichDocument { .. } => "rich_document",
            Self::Group { .. } => "group",
            Self::Unsupported { miro_type } => miro_type,
        }
    }
}

/// A crop rectangle in source-image pixels.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Crop {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// One imported widget.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Widget {
    /// Miro's own widget id, kept so a re-import can update rather than duplicate.
    pub miro_id: Option<String>,
    /// Miro's `_parent` — the frame or group this belongs to.
    pub parent: Option<String>,
    /// The parent as an index into the payload's objects array. Miro references
    /// objects positionally, so this is the form the hierarchy actually resolves
    /// through; `parent` is the same value stringified for display.
    pub parent_index: Option<usize>,
    pub placement: Placement,
    pub kind: WidgetKind,
    /// The untouched Miro JSON. Retained so nothing is lost to a mapping gap and
    /// so new fields can be recovered later without a re-export.
    pub raw: serde_json::Value,
}

/// What did and did not survive an import. Surfaced to the user after every paste,
/// because a silent partial import is the failure mode that erodes trust fastest.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FidelityReport {
    /// Count per widget label, including unsupported types under their Miro name.
    pub counts: Vec<(String, usize)>,
    /// Widgets whose asset is referenced but whose bytes we do not hold locally.
    pub missing_assets: Vec<String>,
    /// Non-fatal problems worth showing the user.
    pub warnings: Vec<String>,
}

impl FidelityReport {
    pub fn total(&self) -> usize {
        self.counts.iter().map(|(_, n)| n).sum()
    }

    /// Number of widgets that imported as an unmapped type.
    pub fn unsupported(&self, widgets: &[Widget]) -> usize {
        widgets
            .iter()
            .filter(|w| matches!(w.kind, WidgetKind::Unsupported { .. }))
            .count()
    }
}

/// Renders the report as the short summary shown after an import.
impl std::fmt::Display for FidelityReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "{} widgets imported", self.total())?;
        let mut counts = self.counts.clone();
        counts.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        for (label, n) in &counts {
            writeln!(f, "  {n:>5}  {label}")?;
        }
        if !self.missing_assets.is_empty() {
            writeln!(f, "  {} assets not available locally", self.missing_assets.len())?;
        }
        for w in &self.warnings {
            writeln!(f, "  warning: {w}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The colour mapping is load-bearing and independently checkable: the SVG
    /// export of the same board renders this sticky as `#fff79e`.
    #[test]
    fn sticky_yellow_matches_the_svg_export() {
        assert_eq!(Rgb::from_miro(16775070).unwrap().to_hex(), "#fff79e");
    }

    #[test]
    fn negative_one_is_no_colour_not_black() {
        assert_eq!(Rgb::from_miro(-1), None);
        assert_eq!(Rgb::from_miro(0x1000000), None);
    }

    #[test]
    fn colour_endpoints_decode() {
        assert_eq!(Rgb::from_miro(0).unwrap().to_hex(), "#000000");
        assert_eq!(Rgb::from_miro(0xFFFFFF).unwrap().to_hex(), "#ffffff");
        assert_eq!(Rgb::from_miro(1710618).unwrap().to_hex(), "#1a1a1a");
    }

    #[test]
    fn report_totals_and_renders() {
        let r = FidelityReport {
            counts: vec![("sticky".into(), 44), ("ink".into(), 134)],
            missing_assets: vec!["123".into()],
            warnings: vec!["2 unmapped types".into()],
        };
        assert_eq!(r.total(), 178);
        let s = r.to_string();
        assert!(s.contains("178 widgets imported"));
        assert!(s.contains("134  ink"), "{s}");
        assert!(s.contains("warning: 2 unmapped types"));
    }
}
