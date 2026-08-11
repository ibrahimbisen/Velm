//! Maps decoded Miro JSON onto [`Widget`]s.
//!
//! Two passes, because Miro's coordinates are not self-contained:
//!
//! 1. **Classify** each object into a [`WidgetKind`], recording its raw placement
//!    and its parent index.
//! 2. **Resolve** placements to absolute world space by walking the parent chain.
//!
//! The second pass is not optional. On the reference board **485 of 596 widgets
//! (81%)** use `_position.schema == "parentOffsetPx"`, measured from the parent
//! frame's top-left. Treating those as absolute puts four-fifths of a board in the
//! wrong place while still looking superficially plausible.
//!
//! Miro references objects **by array index**, and `id == index` for every object
//! observed. That applies to `_parent.index`, connector `widgetIndex`, and group
//! `items`.
//!
//! Style is a *JSON string* of compact keys; the dictionary in [`style`] was
//! recovered from real payloads and cross-checked against the SVG export. See
//! `docs/02-miro-formats.md` for evidence and per-key confidence.

use crate::miro_model::*;
use serde_json::Value;
use std::collections::BTreeMap;

/// Object-level `type` codes, distinct from `widgetData.type`.
mod object_type {
    /// A normal widget, carrying `widgetData`.
    pub const WIDGET: i64 = 14;
    /// A group, carrying `items: [index, …]` and no `widgetData`.
    pub const GROUP: i64 = 10;
}

/// Compact style keys, named so call sites read as intent rather than magic strings.
mod style {
    /// Sticky background. Verified: 16775070 → `#fff79e`, matching the SVG export.
    pub const STICKY_BG: &str = "sbc";
    pub const TEXT_COLOR: &str = "tc";
    pub const FONT_FAMILY: &str = "ffn";
    /// Font size; 0 with `fsa: 1` means auto-fit.
    pub const FONT_SIZE: &str = "fs";
    /// Horizontal align: `l` | `c` | `r`.
    pub const ALIGN: &str = "ta";
    /// Line height, as a multiple of font size.
    pub const LINE_HEIGHT: &str = "lh";
    pub const BOLD: &str = "b";
    pub const ITALIC: &str = "i";
    pub const UNDERLINE: &str = "u";
    pub const STRIKE: &str = "s";
    /// Ink/line colour, and connector line colour.
    pub const LINE_COLOR: &str = "lc";
    /// Ink/line thickness in px.
    pub const THICKNESS: &str = "t";
    /// Ink opacity, 0–1.
    pub const INK_OPACITY: &str = "lo";
    /// Frame background colour.
    pub const FRAME_BG: &str = "bc";
    /// Connector dash pattern.
    pub const LINE_DASH: &str = "ls";
    /// Connector routing mode.
    pub const LINE_ROUTING: &str = "lt";
    /// Connector arrowheads.
    pub const ARROW_START: &str = "a_start";
    pub const ARROW_END: &str = "a_end";
    /// Connector crossing hops.
    pub const JUMP_OVERS: &str = "jump";
}

/// Raw placement as Miro expressed it, before the parent chain is resolved.
struct RawPlacement {
    offset: (f64, f64),
    /// True when the offset is relative to the parent's top-left corner.
    parent_relative: bool,
    scale: f64,
    rotation: f64,
    width: Option<f64>,
    height: Option<f64>,
}

/// Maps every object in a decoded payload, and reports what happened.
pub fn map_objects(objects: &[Value]) -> (Vec<Widget>, FidelityReport) {
    // Pass 1 — classify, keeping placements in Miro's own coordinate space.
    let mut raws = Vec::with_capacity(objects.len());
    let mut widgets = Vec::with_capacity(objects.len());
    for obj in objects {
        let (widget, raw) = classify(obj);
        raws.push(raw);
        widgets.push(widget);
    }

    // Pass 2 — resolve to absolute world space.
    let parents: Vec<Option<usize>> = widgets.iter().map(|w| w.parent_index).collect();
    for i in 0..widgets.len() {
        let (x, y) = resolve_absolute(i, &raws, &parents, objects.len());
        widgets[i].placement = Placement {
            x,
            y,
            scale: raws[i].scale,
            rotation: raws[i].rotation,
            width: raws[i].width,
            height: raws[i].height,
        };
    }

    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for w in &widgets {
        *counts.entry(w.kind.label().to_string()).or_default() += 1;
    }

    let mut unsupported: Vec<&str> = widgets
        .iter()
        .filter_map(|w| match &w.kind {
            WidgetKind::Unsupported { miro_type } => Some(miro_type.as_str()),
            _ => None,
        })
        .collect();

    let mut warnings = Vec::new();
    if !unsupported.is_empty() {
        let n = unsupported.len();
        unsupported.sort_unstable();
        unsupported.dedup();
        warnings.push(format!(
            "{n} widget(s) of unmapped type ({}) kept as raw data — they will not render yet",
            unsupported.join(", ")
        ));
    }

    let report = FidelityReport {
        counts: counts.into_iter().collect(),
        missing_assets: Vec::new(), // filled once joined against a .rtb archive
        warnings,
    };
    (widgets, report)
}

/// Walks the parent chain, converting a parent-relative offset into an absolute
/// world position.
///
/// A child's offset is expressed in its parent's **local, unscaled** coordinate
/// space, measured from the parent's **top-left**, while positions denote
/// **centres**. So one step of the walk is
///
/// ```text
/// absolute(child) = absolute(parent) + offset(child) × scale(parent)
///                                    − size(parent) × scale(parent) / 2
/// ```
///
/// Both scale factors matter and neither is cosmetic. Verified against Miro's own
/// SVG export, joined widget-by-widget on `initialId`: under this rule **313 of 313**
/// sized widgets land within 1px of where Miro drew them, and the 39 ink strokes
/// attached to scaled parents move from a median error of **245px** (max 641px) to
/// **0.3px**. The 44 stickies differ by a constant 11 local units, which is the
/// drop-shadow margin baked into the SVG's `#StickerType1` symbol, not a placement
/// error — it scales exactly with the sticky's own scale factor.
///
/// `scale` is Miro's absolute world scale (`scale.scale`), not `relativeScale`, so
/// each step uses its own parent's factor rather than a running product.
///
/// `depth_budget` bounds the walk so a malformed payload with a parent cycle
/// terminates instead of hanging the import.
fn resolve_absolute(
    index: usize,
    raws: &[RawPlacement],
    parents: &[Option<usize>],
    depth_budget: usize,
) -> (f64, f64) {
    let (mut x, mut y) = (0.0, 0.0);
    let mut current = index;
    let mut hops = 0;

    loop {
        let child = &raws[current];
        // A self- or out-of-range reference would otherwise loop forever.
        let parent = parents[current]
            .filter(|_| child.parent_relative)
            .filter(|&p| p < raws.len() && p != current)
            .filter(|_| hops <= depth_budget);

        let Some(parent) = parent else {
            // The chain ends at a widget whose own offset is already absolute.
            return (x + child.offset.0, y + child.offset.1);
        };

        let p = &raws[parent];
        x += child.offset.0 * p.scale - p.width.unwrap_or(0.0) * p.scale / 2.0;
        y += child.offset.1 * p.scale - p.height.unwrap_or(0.0) * p.scale / 2.0;
        current = parent;
        hops += 1;
    }
}

/// Classifies one object without resolving coordinates.
fn classify(obj: &Value) -> (Widget, RawPlacement) {
    let object_type = obj.get("type").and_then(Value::as_i64).unwrap_or(object_type::WIDGET);

    // Groups arrive as a distinct object shape with no `widgetData`.
    if object_type == object_type::GROUP {
        let children = obj
            .get("items")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|v| v.as_u64().map(|n| n as usize)).collect())
            .unwrap_or_default();
        return (
            Widget {
                miro_id: obj.get("id").and_then(as_id),
                parent: None,
                parent_index: None,
                placement: Placement::default(),
                kind: WidgetKind::Group { children },
                raw: obj.clone(),
            },
            RawPlacement {
                offset: (0.0, 0.0),
                parent_relative: false,
                scale: 1.0,
                rotation: 0.0,
                width: None,
                height: None,
            },
        );
    }

    let data = obj.get("widgetData");
    let json = data.and_then(|d| d.get("json")).unwrap_or(&Value::Null);
    let miro_type = data.and_then(|d| d.get("type")).and_then(Value::as_str).unwrap_or("");

    let kind = match miro_type {
        "sticker" => WidgetKind::Sticky {
            html: str_field(json, "text").unwrap_or_default(),
            background: style_color(json, style::STICKY_BG),
            style: text_style(json),
        },
        "text" => WidgetKind::Text {
            html: str_field(json, "text").unwrap_or_default(),
            style: text_style(json),
        },
        "paint" => WidgetKind::Ink(Ink {
            points: points(json),
            color: style_color(json, style::LINE_COLOR),
            thickness: style_num(json, style::THICKNESS),
            opacity: style_num(json, style::INK_OPACITY),
        }),
        "image" => WidgetKind::Image { asset: asset(json), crop: crop(json) },
        "document" => WidgetKind::Document { asset: asset(json) },
        "embed" => {
            let cd = json.get("custom_data").unwrap_or(&Value::Null);
            WidgetKind::Embed {
                title: str_field(cd, "title"),
                url: str_field(cd, "url"),
                description: str_field(cd, "description"),
                // Two sources for one field: the widget's own `provider` object is
                // present on all 40 of the reference board's embeds, `custom_data`'s
                // flat `provider_name` on the same 40. Preferring the object matches
                // what Miro shows; the flat one is the fallback for a payload that
                // carries only oEmbed's own keys.
                provider: json
                    .get("provider")
                    .and_then(|p| str_field(p, "name"))
                    .or_else(|| str_field(cd, "provider_name")),
                html: str_field(cd, "html"),
                // `thumbnail_url` is oEmbed's own field and is present on only 26 of
                // the 40, where `resourceWidget` is on all 40 — so it is the fallback,
                // not the source.
                image: resource_widget(json, str_field(cd, "thumbnail_url")),
            }
        }
        "frame" => WidgetKind::Frame {
            // Frames carry their name in `text`, e.g. "Sensors".
            title: str_field(json, "text").unwrap_or_default(),
            background: style_color(json, style::FRAME_BG),
            order: json.get("prevFrameIndex").and_then(Value::as_i64),
            speaker_notes: str_field(json, "speakerNotes"),
        },
        "line" => WidgetKind::Connector {
            start: connector_end(json, "primary", style::ARROW_START),
            end: connector_end(json, "secondary", style::ARROW_END),
            style: ConnectorStyle {
                color: style_color(json, style::LINE_COLOR),
                thickness: style_num(json, style::THICKNESS),
                dash: style_int(json, style::LINE_DASH),
                routing: style_int(json, style::LINE_ROUTING),
                jump_overs: style_int(json, style::JUMP_OVERS).unwrap_or(0) != 0,
            },
            captions: json
                .get("line")
                .and_then(|l| l.get("captions"))
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(caption_text).collect())
                .unwrap_or_default(),
        },
        "preview" => {
            let og = json.get("openGraph").unwrap_or(&Value::Null);
            WidgetKind::LinkPreview {
                title: str_field(og, "title"),
                url: str_field(og, "url")
                    .or_else(|| json.get("path").and_then(|p| str_field(p, "path"))),
                description: str_field(og, "description"),
                // `openGraph.images` is present on only 4 of the reference board's 91
                // previews while `resourceWidget` is on 64, because Miro downloads the
                // picture and keeps its own copy rather than re-hitting the origin.
                // Ours is the same instinct.
                image: resource_widget(
                    json,
                    og.get("images")
                        .and_then(Value::as_array)
                        .and_then(|a| a.first())
                        .and_then(|i| str_field(i, "url").or_else(|| i.as_str().map(str::to_string))),
                ),
                visual_type: json.get("visualType").and_then(Value::as_i64),
            }
        }
        "structured_document" => WidgetKind::RichDocument { ops: delta_ops(json) },
        other => WidgetKind::Unsupported { miro_type: other.to_string() },
    };

    let raw_placement = raw_placement(json);
    let widget = Widget {
        miro_id: obj.get("id").and_then(as_id),
        parent: json.get("_parent").and_then(parent_ref).map(|i| i.to_string()),
        parent_index: json.get("_parent").and_then(parent_ref),
        placement: Placement::default(), // filled by pass 2
        kind,
        raw: json.clone(),
    };
    (widget, raw_placement)
}

/// Reads `_parent`, which Miro expresses as `{"index": n}` referencing the objects
/// array. `id == index` for every object observed, so the two are interchangeable.
fn parent_ref(v: &Value) -> Option<usize> {
    match v {
        Value::Object(_) => v.get("index")?.as_u64().map(|n| n as usize),
        Value::Number(n) => n.as_u64().map(|n| n as usize),
        _ => None,
    }
}

fn raw_placement(json: &Value) -> RawPlacement {
    let position = json.get("_position").filter(|p| !p.is_null());
    let offset = position.and_then(|p| p.get("offsetPx"));
    let schema = position.and_then(|p| p.get("schema")).and_then(Value::as_str);
    let size = json.get("size");

    RawPlacement {
        offset: (
            offset.and_then(|o| o.get("x")).and_then(Value::as_f64).unwrap_or(0.0),
            offset.and_then(|o| o.get("y")).and_then(Value::as_f64).unwrap_or(0.0),
        ),
        parent_relative: schema == Some("parentOffsetPx"),
        scale: json
            .get("scale")
            .and_then(|s| s.get("scale"))
            .and_then(Value::as_f64)
            .or_else(|| json.get("relativeScale").and_then(Value::as_f64))
            .unwrap_or(1.0),
        rotation: json
            .get("rotation")
            .and_then(|r| r.get("rotation"))
            .and_then(Value::as_f64)
            .or_else(|| json.get("relativeRotation").and_then(Value::as_f64))
            .unwrap_or(0.0),
        // Frames carry flat `width`/`height` alongside `size`. Images carry
        // neither: their extent is the *crop* rectangle (falling back to the
        // source resource), which is what Miro lays them out by — and what a
        // child of an image resolves its offset against.
        width: size_field(json, size, "width"),
        height: size_field(json, size, "height"),
    }
}

/// Reads one dimension from wherever the widget type happens to keep it.
fn size_field(json: &Value, size: Option<&Value>, key: &str) -> Option<f64> {
    size.and_then(|s| s.get(key))
        .and_then(Value::as_f64)
        .or_else(|| json.get(key).and_then(Value::as_f64))
        .or_else(|| json.get("crop").and_then(|c| c.get(key)).and_then(Value::as_f64))
        .or_else(|| json.get("resource").and_then(|r| r.get(key)).and_then(Value::as_f64))
}

/// Reads one connector endpoint. `widgetIndex` binds the end to another object, so
/// the connector can re-route when that object moves.
fn connector_end(json: &Value, side: &str, arrow_key: &str) -> ConnectorEnd {
    let e = json.get(side).unwrap_or(&Value::Null);
    ConnectorEnd {
        target: e.get("widgetIndex").and_then(Value::as_u64).map(|n| n as usize),
        anchor: (
            e.get("point").and_then(|p| p.get("x")).and_then(Value::as_f64).unwrap_or(0.5),
            e.get("point").and_then(|p| p.get("y")).and_then(Value::as_f64).unwrap_or(0.5),
        ),
        arrowhead: style_int(json, arrow_key).unwrap_or(0),
    }
}

/// Connector captions have not been observed populated; accept a bare string or a
/// `{text: …}` object so whichever shape appears is captured rather than dropped.
fn caption_text(c: &Value) -> Option<String> {
    match c {
        Value::String(s) => Some(s.clone()),
        Value::Object(_) => str_field(c, "text").or_else(|| str_field(c, "content")),
        _ => None,
    }
}

/// Reads Quill delta ops from a `structured_document`.
fn delta_ops(json: &Value) -> Vec<DeltaOp> {
    json.get("content")
        .and_then(Value::as_array)
        .map(|ops| {
            ops.iter()
                .filter_map(|op| {
                    Some(DeltaOp {
                        insert: op.get("insert")?.as_str()?.to_string(),
                        attributes: op.get("attributes").cloned(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Miro ids appear as both strings and numbers depending on the field.
fn as_id(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn str_field(v: &Value, key: &str) -> Option<String> {
    v.get(key)?.as_str().map(str::to_string)
}

/// Parses the `style` field, which Miro stores as a JSON *string*.
fn style_obj(json: &Value) -> Option<Value> {
    let raw = json.get("style")?;
    match raw {
        // A string in every payload so far; tolerate a plain object too, so a
        // future format change degrades to working rather than blank.
        Value::String(s) => serde_json::from_str(s).ok(),
        Value::Object(_) => Some(raw.clone()),
        _ => None,
    }
}

fn style_num(json: &Value, key: &str) -> Option<f64> {
    style_obj(json)?.get(key)?.as_f64()
}

fn style_int(json: &Value, key: &str) -> Option<i64> {
    style_obj(json)?.get(key)?.as_i64()
}

fn style_color(json: &Value, key: &str) -> Option<Rgb> {
    Rgb::from_miro(style_obj(json)?.get(key)?.as_i64()?)
}

fn text_style(json: &Value) -> TextStyle {
    let s = style_obj(json).unwrap_or(Value::Null);
    let flag = |k: &str| s.get(k).and_then(Value::as_i64).unwrap_or(0) != 0;
    TextStyle {
        font_family: str_field(&s, style::FONT_FAMILY),
        // 0 means auto-fit, which is absence of an explicit size.
        font_size: s.get(style::FONT_SIZE).and_then(Value::as_f64).filter(|v| *v > 0.0),
        color: s.get(style::TEXT_COLOR).and_then(Value::as_i64).and_then(Rgb::from_miro),
        align: match s.get(style::ALIGN).and_then(Value::as_str) {
            Some("l") => Some(Align::Left),
            Some("c") => Some(Align::Center),
            Some("r") => Some(Align::Right),
            _ => None,
        },
        line_height: s.get(style::LINE_HEIGHT).and_then(Value::as_f64),
        bold: flag(style::BOLD),
        italic: flag(style::ITALIC),
        underline: flag(style::UNDERLINE),
        strikethrough: flag(style::STRIKE),
    }
}

fn points(json: &Value) -> Vec<(f64, f64)> {
    json.get("points")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|p| Some((p.get("x")?.as_f64()?, p.get("y")?.as_f64()?)))
                .collect()
        })
        .unwrap_or_default()
}

fn asset(json: &Value) -> AssetRef {
    let r = json.get("resource").unwrap_or(&Value::Null);
    AssetRef {
        id: r.get("id").and_then(as_id).unwrap_or_default(),
        name: str_field(r, "name"),
        width: r.get("width").and_then(Value::as_f64),
        height: r.get("height").and_then(Value::as_f64),
        external_url: json
            .get("document")
            .and_then(|d| d.get("externalLink"))
            .and_then(Value::as_str)
            .or_else(|| json.get("image").and_then(|i| i.get("externalLink")).and_then(Value::as_str))
            .map(str::to_string),
    }
}

/// A link card's preview image, from the `resourceWidget` Miro attaches once it has
/// fetched the page.
///
/// Shaped differently from [`asset`] and so decoded separately: the id and the
/// dimensions live under `meta`, not beside `id`, and there is no `resource` wrapper.
/// `fallback_url` is the card's own thumbnail field, used only when the widget carries
/// no resource at all.
///
/// **`meta.externalLink` is the load-bearing part.** It is the origin CDN URL —
/// `i.ytimg.com/vi/…/hqdefault.jpg`, `parts.example.com/public/assets/…JPG` — so a board
/// pasted with no `.rtb` beside it can still fetch the picture from a plain image host
/// rather than from a product page that answers 403 to anything without a browser's
/// user agent. That is the difference between a card with a picture and a grey box for
/// the 49 of 131 links on the reference board whose *pages* cannot be fetched at all.
fn resource_widget(json: &Value, fallback_url: Option<String>) -> Option<AssetRef> {
    let Some(rw) = json.get("resourceWidget").filter(|v| !v.is_null()) else {
        // No stored resource: a bare URL is still worth carrying, since the fetch pool
        // can turn it into bytes later.
        return fallback_url.map(|url| AssetRef {
            id: String::new(),
            name: None,
            width: None,
            height: None,
            external_url: Some(url),
        });
    };
    let meta = rw.get("meta").unwrap_or(&Value::Null);
    Some(AssetRef {
        id: rw.get("id").and_then(as_id).unwrap_or_default(),
        name: str_field(rw, "name"),
        width: meta.get("width").and_then(Value::as_f64),
        height: meta.get("height").and_then(Value::as_f64),
        external_url: str_field(meta, "externalLink").or(fallback_url),
    })
}

fn crop(json: &Value) -> Option<Crop> {
    let c = json.get("crop")?;
    Some(Crop {
        x: c.get("x")?.as_f64()?,
        y: c.get("y")?.as_f64()?,
        width: c.get("width")?.as_f64()?,
        height: c.get("height")?.as_f64()?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn obj(ty: &str, json: Value) -> Value {
        json!({ "id": "42", "type": 14, "widgetData": { "type": ty, "json": json } })
    }

    fn one(v: Value) -> Widget {
        map_objects(std::slice::from_ref(&v)).0.remove(0)
    }

    /// Mirrors a real sticky captured from the clipboard, style string included.
    #[test]
    fn maps_a_real_sticky() {
        let w = one(obj(
            "sticker",
            json!({
                "_position": { "offsetPx": { "x": -3083.01852968025, "y": 1367.540251981647 }, "schema": "canvasOffsetPx" },
                "scale": { "scale": 1.85 },
                "size": { "width": 199, "height": 228 },
                "text": "<p>fan</p><p><br /></p>",
                "style": "{\"fs\":0,\"fsa\":1,\"ffn\":\"Noto Sans\",\"ta\":\"c\",\"lh\":1.36,\"sbc\":16775070}"
            }),
        ));
        assert_eq!(w.placement.scale, 1.85);
        assert_eq!(w.placement.x, -3083.01852968025);
        let WidgetKind::Sticky { html, background, style } = &w.kind else { panic!() };
        assert_eq!(html, "<p>fan</p><p><br /></p>");
        assert_eq!(background.unwrap().to_hex(), "#fff79e");
        assert_eq!(style.align, Some(Align::Center));
        assert_eq!(style.font_size, None, "fs:0 + fsa:1 is auto-fit, not zero");
    }

    /// The regression that matters most: a child's offset is measured from its
    /// parent's top-left, and positions denote centres.
    #[test]
    fn resolves_parent_relative_coordinates_to_absolute() {
        // Frame centred at (100, 200), 400x100 → top-left is (-100, 150).
        let frame = obj(
            "frame",
            json!({
                "_position": { "offsetPx": { "x": 100.0, "y": 200.0 }, "schema": "canvasOffsetPx" },
                "width": 400.0, "height": 100.0, "text": "Sensors"
            }),
        );
        let child = json!({
            "id": "1", "type": 14,
            "widgetData": { "type": "sticker", "json": {
                "_position": { "offsetPx": { "x": 10.0, "y": 20.0 }, "schema": "parentOffsetPx" },
                "_parent": { "index": 0 }, "text": "x"
            }}
        });
        let (w, _) = map_objects(&[frame, child]);
        assert_eq!(w[0].placement.x, 100.0, "the frame itself is absolute");
        assert_eq!((w[1].placement.x, w[1].placement.y), (-90.0, 170.0));
        assert_eq!(w[1].parent_index, Some(0));
    }

    /// Nesting must compose, not just resolve one level.
    #[test]
    fn resolves_nested_parents() {
        let outer = obj("frame", json!({
            "_position": { "offsetPx": { "x": 0.0, "y": 0.0 }, "schema": "canvasOffsetPx" },
            "width": 200.0, "height": 200.0 }));
        let mid = json!({ "id": "1", "type": 14, "widgetData": { "type": "frame", "json": {
            "_position": { "offsetPx": { "x": 10.0, "y": 10.0 }, "schema": "parentOffsetPx" },
            "_parent": { "index": 0 }, "width": 50.0, "height": 50.0 }}});
        let leaf = json!({ "id": "2", "type": 14, "widgetData": { "type": "sticker", "json": {
            "_position": { "offsetPx": { "x": 5.0, "y": 5.0 }, "schema": "parentOffsetPx" },
            "_parent": { "index": 1 }, "text": "x" }}});
        let (w, _) = map_objects(&[outer, mid, leaf]);
        // mid: (10,10) + (0 - 100) = (-90, -90)
        assert_eq!((w[1].placement.x, w[1].placement.y), (-90.0, -90.0));
        // leaf: (5,5) + (-90 - 25) = (-110, -110)
        assert_eq!((w[2].placement.x, w[2].placement.y), (-110.0, -110.0));
    }

    /// The regression that hid behind the first one: a child's offset is in its
    /// parent's *unscaled* space, so both the offset and the parent's half-extent
    /// scale with the parent.
    ///
    /// These are the real numbers for clipboard objects 266 (a sticky, 199×228 at
    /// scale 2.84096808336024, centred at (-1041.2825, 1891.7505)) and 268, an ink
    /// stroke drawn on it. Miro's SVG export puts that stroke's 33.14×72.60 box at
    /// (-1263.27, 1631.46); ignoring the parent's scale lands it 245px away.
    #[test]
    fn parent_scale_applies_to_both_the_offset_and_the_parent_extent() {
        let sticky = obj(
            "sticker",
            json!({
                "_position": { "offsetPx": { "x": -1041.2825, "y": 1891.7505 }, "schema": "canvasOffsetPx" },
                "scale": { "scale": 2.84096808336024 },
                "size": { "width": 199, "height": 228 },
                "text": "<p>fan</p>"
            }),
        );
        let stroke = json!({ "id": "1", "type": 14, "widgetData": { "type": "paint", "json": {
            "_position": { "offsetPx": { "x": 27.19707433986703, "y": 35.15825531795441 },
                           "containerType": "CURVE_HOLDER", "schema": "parentOffsetPx" },
            "_parent": { "index": 0 }, "scale": { "scale": 1.0 }, "relativeScale": 0.3519926907511119,
            "points": [{ "x": 0, "y": 0 }, { "x": 33.14, "y": 72.60 }]
        }}});

        let (w, _) = map_objects(&[sticky, stroke]);
        // Ink positions are the centre of the stroke's own box, so the SVG's
        // top-left (-1263.27, 1631.46) plus half of 33.14 × 72.60.
        assert!((w[1].placement.x - (-1263.27 + 33.14 / 2.0)).abs() < 0.1, "{:?}", w[1].placement);
        assert!((w[1].placement.y - (1631.46 + 72.60 / 2.0)).abs() < 0.1, "{:?}", w[1].placement);
    }

    /// Images carry no `size`; their extent is the crop rectangle. Without it a
    /// child of an image resolves against a zero-width parent and lands half an
    /// image away.
    #[test]
    fn images_take_their_extent_from_the_crop_rectangle() {
        let image = obj(
            "image",
            json!({
                "_position": { "offsetPx": { "x": 0.0, "y": 0.0 }, "schema": "canvasOffsetPx" },
                "scale": { "scale": 2.0 },
                "crop": { "x": 0, "y": 0, "width": 722, "height": 841, "shape": "custom" },
                "resource": { "id": "1", "name": "image.png", "width": 900, "height": 1000 }
            }),
        );
        let stroke = json!({ "id": "1", "type": 14, "widgetData": { "type": "paint", "json": {
            "_position": { "offsetPx": { "x": 100.0, "y": 200.0 }, "schema": "parentOffsetPx" },
            "_parent": { "index": 0 }, "points": [] }}});

        let (w, _) = map_objects(&[image, stroke]);
        assert_eq!(w[0].placement.width, Some(722.0), "crop wins over the source resource");
        // (100, 200) × 2 − (722, 841) × 2 / 2
        assert_eq!((w[1].placement.x, w[1].placement.y), (-522.0, -441.0));
    }

    /// A malformed payload must not hang the import.
    #[test]
    fn parent_cycles_terminate() {
        let mk = |id: &str, parent: usize| json!({ "id": id, "type": 14,
            "widgetData": { "type": "sticker", "json": {
                "_position": { "offsetPx": { "x": 1.0, "y": 1.0 }, "schema": "parentOffsetPx" },
                "_parent": { "index": parent }, "text": "x" }}});
        let (w, _) = map_objects(&[mk("0", 1), mk("1", 0)]);
        assert_eq!(w.len(), 2); // terminated rather than looping
    }

    #[test]
    fn maps_a_real_frame() {
        let w = one(obj("frame", json!({
            "_position": { "offsetPx": { "x": -3043.1, "y": 3630.7 }, "schema": "canvasOffsetPx" },
            "width": 2603.65, "height": 4240.41, "text": "Sensors", "prevFrameIndex": 9,
            "speakerNotes": null,
            "style": "{\"fs\":14,\"tc\":6052956,\"bc\":16777215}"
        })));
        let WidgetKind::Frame { title, background, order, .. } = &w.kind else { panic!() };
        assert_eq!(title, "Sensors");
        assert_eq!(*order, Some(9));
        assert_eq!(background.unwrap().to_hex(), "#ffffff");
        assert_eq!(w.placement.width, Some(2603.65), "frames use flat width/height");
    }

    /// Connectors bind to widgets by index; that binding is why importing them
    /// is worth anything.
    #[test]
    fn maps_a_real_connector_with_endpoint_bindings() {
        let w = one(obj("line", json!({
            "points": [],
            "primary": { "point": { "x": 1, "y": 0.5 }, "positionType": 0, "widgetIndex": 219 },
            "secondary": { "point": { "x": 0, "y": 0.5 }, "positionType": 0, "widgetIndex": 220 },
            "_position": null,
            "style": "{\"lc\":3355443,\"ls\":2,\"t\":2,\"lt\":1,\"a_start\":0,\"a_end\":9,\"jump\":0}",
            "line": { "captions": [] }
        })));
        let WidgetKind::Connector { start, end, style, captions } = &w.kind else { panic!() };
        assert_eq!(start.target, Some(219));
        assert_eq!(end.target, Some(220));
        assert_eq!(start.anchor, (1.0, 0.5));
        assert_eq!(start.arrowhead, 0);
        assert_eq!(end.arrowhead, 9);
        assert_eq!(style.thickness, Some(2.0));
        assert_eq!(style.color.unwrap().to_hex(), "#333333");
        assert!(!style.jump_overs);
        assert!(captions.is_empty());
    }

    #[test]
    fn maps_a_real_link_preview() {
        let w = one(obj("preview", json!({
            "visualType": 0,
            "openGraph": { "title": "Acme F90 M5 Shifter", "description": "8HP shifter…",
                           "url": "https://wiki.example.net/x" },
            "path": { "path": "https://wiki.example.net/x" },
            "size": { "width": 250, "height": 190 }
        })));
        let WidgetKind::LinkPreview { title, url, .. } = &w.kind else { panic!() };
        assert_eq!(title.as_deref(), Some("Acme F90 M5 Shifter"));
        assert_eq!(url.as_deref(), Some("https://wiki.example.net/x"));
    }

    #[test]
    fn maps_a_rich_document_from_quill_deltas() {
        let w = one(obj("structured_document", json!({
            "content": [
                { "insert": "https://example.com", "attributes": { "color": "#3578ff" } },
                { "insert": "\n", "attributes": { "header": 1 } }
            ]
        })));
        let WidgetKind::RichDocument { ops } = &w.kind else { panic!() };
        assert_eq!(ops.len(), 2);
        assert_eq!(ops[0].insert, "https://example.com");
        assert_eq!(ops[0].attributes.as_ref().unwrap()["color"], "#3578ff");
    }

    /// Groups arrive as a distinct object shape with no `widgetData` at all.
    #[test]
    fn maps_groups_which_have_no_widget_data() {
        let w = one(json!({ "type": 10, "items": [266, 267], "id": 595,
                            "initialId": "3458764500000000001" }));
        assert_eq!(w.kind, WidgetKind::Group { children: vec![266, 267] });
    }

    #[test]
    fn maps_ink_with_stroke_style() {
        let w = one(obj("paint", json!({
            "_position": { "offsetPx": { "x": -3351.5, "y": -575.4 }, "schema": "canvasOffsetPx" },
            "points": [{"x": 0, "y": 0}, {"x": 10.5, "y": -4.25}],
            "style": "{\"lc\":3000156,\"t\":18,\"lo\":1}"
        })));
        let WidgetKind::Ink(ink) = &w.kind else { panic!() };
        assert_eq!(ink.points, vec![(0.0, 0.0), (10.5, -4.25)]);
        assert_eq!(ink.thickness, Some(18.0));
        assert_eq!(ink.color.unwrap().to_hex(), "#2dc75c"); // 3000156 == 0x2DC75C
    }

    #[test]
    fn image_keeps_the_rtb_join_key_and_crop() {
        let w = one(obj("image", json!({
            "crop": { "x": 0, "y": 0, "width": 1920, "height": 1080, "shape": "custom" },
            "resource": { "id": "3458764500000000002", "name": "image.png", "width": 1920, "height": 1080 }
        })));
        let WidgetKind::Image { asset, crop } = &w.kind else { panic!() };
        assert_eq!(asset.id, "3458764500000000002");
        assert_eq!(crop.unwrap().width, 1920.0);
    }

    #[test]
    fn embed_keeps_link_metadata() {
        let w = one(obj("embed", json!({
            "custom_data": { "title": "GitHub - x/superposition", "url": "https://github.com/x/s", "description": "d" }
        })));
        let WidgetKind::Embed { title, url, .. } = &w.kind else { panic!() };
        assert_eq!(title.as_deref(), Some("GitHub - x/superposition"));
        assert_eq!(url.as_deref(), Some("https://github.com/x/s"));
    }

    #[test]
    fn unknown_types_are_preserved_and_reported() {
        let v = obj("mindmap_node", json!({ "_position": { "offsetPx": { "x": 5, "y": 6 } }, "custom": 1 }));
        let (w, report) = map_objects(std::slice::from_ref(&v));
        assert_eq!(w[0].kind, WidgetKind::Unsupported { miro_type: "mindmap_node".into() });
        assert_eq!(w[0].placement.x, 5.0);
        assert_eq!(w[0].raw.get("custom").unwrap(), 1);
        assert_eq!(report.unsupported(&w), 1);
        assert!(report.warnings[0].contains("mindmap_node"));
    }

    /// A paste is user-triggered and must never take the app down.
    #[test]
    fn missing_and_malformed_fields_degrade_gracefully() {
        let (w, _) = map_objects(&[json!({}), obj("sticker", json!({ "style": "not json" }))]);
        assert_eq!(w[0].placement.x, 0.0);
        assert_eq!(w[0].placement.scale, 1.0);
        let WidgetKind::Sticky { background, style, .. } = &w[1].kind else { panic!() };
        assert_eq!(*background, None);
        assert_eq!(style.font_family, None);
    }
}
