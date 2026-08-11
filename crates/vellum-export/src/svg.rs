//! Real vector SVG, shaped so Miro's own reader can read it back.
//!
//! # Why the output looks like Miro's
//!
//! `vellum_import::svg` already streams Miro's SVG export and counts what is in it,
//! and `docs/01-architecture.md` §8 calls that the strongest correctness signal in
//! the project. Emitting the same structural markers turns it into a **round-trip
//! oracle for the exporter too**: write a board, read it back with a parser written
//! against somebody else's format, and the counts must agree. `tests/roundtrip.rs`
//! does exactly that.
//!
//! The markers are not decoration; each one is how a widget's *type* survives being
//! flattened to vector art:
//!
//! | Widget | Marker | Read back as |
//! |---|---|---|
//! | sticky | `<use xlink:href="#StickerType1">` with a `fill` | `stickies`, `sticky_colors` |
//! | frame | `data-frame="true"` | `frames` |
//! | shape | `class="shape-element shape-element-<name>"` | `shapes` |
//! | link preview | `class="preview-widget preview-widget_default"` | `link_previews` |
//! | embed | `class="embed-widget"` | `embeds` |
//! | connector arrowhead | `<use xlink:href="#LineHeadArrow1">` | `connector_arrowheads` |
//! | ink | a long `<path d>` | `ink_paths` |
//! | image | `href="data:image/…;base64,…"` | `embedded_images` |
//! | text | one `<text>` per line | `text_strings` |
//!
//! Two of those need care, and both are documented on the reader as well:
//!
//! - The sticky symbol is defined in `<defs>` as `<g id="StickerType1">` whose child
//!   references `#StickerType1Path`. The *definition* must not look like an
//!   instance, which is why the inner reference carries the `Path` suffix.
//! - Ink is recognised by path length — 500 characters in the reader — so a short
//!   two-point stroke is legitimately invisible to it. That is a floor, not an
//!   equality, and `SvgInventory::compare` treats it as one.
//!
//! # Shapes stay shapes
//!
//! A rectangle is written as `<rect>`, an ellipse as `<ellipse>` and a straight-edged
//! silhouette as `<polygon>` — never as a path. Each is smaller and more accurate
//! than the equivalent `d`, and, since the reader tells ink from decoration by the
//! *length* of a `<path d>`, it also keeps a large shape from reading back as an ink
//! stroke.
//!
//! That heuristic has one honest limit, pinned by
//! `tests/roundtrip.rs::a_curved_shape_outline_can_cross_the_oracles_ink_threshold`:
//! a **curved** shape — a cloud, a heart, a cylinder — has no shorter form than a
//! path, and a large one exceeds 500 characters and is then counted as ink *as well
//! as* as a shape. The shape count stays exact either way. This is a property of a
//! length-based heuristic, which the reader documents as approximate, and not
//! something the writer can fix without abandoning curves.

use crate::error::ExportError;
use crate::geom::{Path, Point, Rect, Segment, num, write_num};
use crate::item::{EndCap, Geometry, Kind};
use crate::scene::{Placed, Scene};
use crate::source::ImageData;
use crate::style::{Color, Style};
use crate::text::{Align, Span};
use base64::Engine as _;
use std::collections::BTreeSet;
use std::fmt::Write as _;

/// How the SVG is written.
#[derive(Debug, Clone)]
pub struct SvgOptions {
    /// Decimal places on coordinates. Two is a hundredth of a board pixel —
    /// finer than any display at any zoom Vellum supports, and it roughly halves
    /// the byte count against the six digits `f64` would print by default.
    pub precision: u8,
    /// Embed image bytes as `data:` URIs. Turning this off writes a placeholder
    /// rectangle instead, for the case where an SVG is wanted for its geometry and
    /// the reference board's 205 images would make it 36MB.
    pub embed_images: bool,
    /// Painted behind everything. `None` leaves the SVG transparent, which is what
    /// you want when dropping it into another document.
    pub background: Option<Color>,
    /// Board units of empty space around the content.
    pub margin: f64,
    /// Newlines and indentation. Off produces a smaller file; on produces one that
    /// can be read and diffed, which is worth more during development.
    pub indent: bool,
    /// Draw each frame's name above its top-left corner, as Miro does.
    ///
    /// Only the SVG writer does this, and the asymmetry is deliberate: in a PDF the
    /// frame *is* the page and in a PNG the frame *is* the image, so a caption
    /// stamped inside either would be content the user never placed. A whole-board
    /// SVG shows many frames at once, where the labels are how you tell them apart.
    pub frame_titles: bool,
}

impl Default for SvgOptions {
    fn default() -> Self {
        Self {
            precision: 2,
            embed_images: true,
            background: None,
            margin: 0.0,
            indent: true,
            frame_titles: true,
        }
    }
}

/// Writes `scene` as an SVG document.
pub fn write(scene: &Scene, options: &SvgOptions) -> Result<String, ExportError> {
    if scene.is_empty() {
        return Err(ExportError::NothingToExport);
    }
    let mut w = Writer::new(options);
    let view = view_box(scene, options);

    w.raw("<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"no\"?>");
    w.raw(&format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" xmlns:xlink=\"http://www.w3.org/1999/xlink\" \
         version=\"1.1\" width=\"{w_}px\" height=\"{h}px\" viewBox=\"{vx} {vy} {w_} {h}\">",
        w_ = num(view.width, options.precision),
        h = num(view.height, options.precision),
        vx = num(view.x, options.precision),
        vy = num(view.y, options.precision),
    ));
    w.depth += 1;

    if let Some(title) = &scene.title {
        w.line(&format!("<title>{}</title>", escape_text(title)));
    }
    let defs = collect_defs(scene);
    write_defs(&mut w, &defs);

    if let Some(background) = options.background {
        w.line(&format!(
            "<rect x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\" fill=\"{}\"{}/>",
            num(view.x, w.precision),
            num(view.y, w.precision),
            num(view.width, w.precision),
            num(view.height, w.precision),
            background.to_hex(),
            opacity_attr("fill-opacity", background.opacity()),
        ));
    }

    for placed in &scene.items {
        write_item(&mut w, scene, &defs, placed, options);
    }

    w.depth -= 1;
    w.line("</svg>");
    Ok(w.out)
}

/// The document's view box.
///
/// Not simply the scene's bounds: a frame's caption is drawn *above* its top edge,
/// outside the frame, so a view box taken from painted geometry alone would cut
/// every frame name off. The scene cannot account for this — the caption is the SVG
/// writer's own decoration, and the PDF and PNG writers do not draw one.
fn view_box(scene: &Scene, options: &SvgOptions) -> Rect {
    let mut view = scene.bounds;
    if options.frame_titles {
        let strip = crate::text::FontSpec::default().size * CAPTION_CLEARANCE;
        for frame in &scene.frames {
            if frame.title.is_some() {
                let rect = frame.rect;
                view = view.union(Rect::new(rect.x, rect.y - strip, rect.width, strip));
            }
        }
    }
    view.inflated(options.margin)
}

/// Vertical space a frame caption needs, in multiples of its font size: the
/// baseline offset below plus room for the ascenders above it.
const CAPTION_CLEARANCE: f64 = 1.3;

/// Where a frame caption's baseline sits, relative to the frame's top edge.
const CAPTION_BASELINE: f64 = 0.4;

/// Writes an SVG to a file.
pub fn write_file(
    scene: &Scene,
    options: &SvgOptions,
    path: impl AsRef<std::path::Path>,
) -> Result<(), ExportError> {
    let path = path.as_ref();
    let svg = write(scene, options)?;
    std::fs::write(path, svg).map_err(|e| ExportError::io(path.display(), e))
}

// ---------------------------------------------------------------------------
// Document skeleton
// ---------------------------------------------------------------------------

/// Ids used inside the document. Collected first so `<defs>` holds only what is
/// actually referenced — a board with no connectors gets no arrowhead definition.
struct Defs {
    sticker_square: bool,
    sticker_wide: bool,
    arrow: bool,
    circle_cap: bool,
    clips: Vec<(usize, Rect)>,
}

/// A sticky is square when its sides are within this fraction of each other.
/// Miro ships two sticky symbols, square and wide, and picking between them by
/// aspect is what keeps a reader's `StickerType1`/`StickerType2` split meaningful.
const SQUARE_TOLERANCE: f64 = 0.08;

fn sticker_id(rect: Rect) -> &'static str {
    let longer = rect.width.max(rect.height);
    let shorter = rect.width.min(rect.height);
    if longer <= 0.0 || (longer - shorter) / longer <= SQUARE_TOLERANCE {
        "StickerType1"
    } else {
        "StickerType2"
    }
}

fn collect_defs(scene: &Scene) -> Defs {
    let mut defs =
        Defs { sticker_square: false, sticker_wide: false, arrow: false, circle_cap: false, clips: Vec::new() };
    let mut seen_clips: BTreeSet<[u64; 4]> = BTreeSet::new();

    for (index, placed) in scene.items.iter().enumerate() {
        match placed.item.kind {
            Kind::Sticky => match sticker_id(placed.item.geometry.bounds()) {
                "StickerType1" => defs.sticker_square = true,
                _ => defs.sticker_wide = true,
            },
            Kind::Connector { start, end } => {
                for cap in [start, end] {
                    match cap {
                        EndCap::Arrow => defs.arrow = true,
                        EndCap::Circle => defs.circle_cap = true,
                        EndCap::None => {}
                    }
                }
            }
            _ => {}
        }
        if let Some(clip) = placed.clip {
            // Frames share clip rectangles, so one `<clipPath>` serves every child
            // of a frame rather than one per item.
            let key = [
                clip.x.to_bits(),
                clip.y.to_bits(),
                clip.width.to_bits(),
                clip.height.to_bits(),
            ];
            if seen_clips.insert(key) {
                defs.clips.push((index, clip));
            }
        }
    }
    defs
}

fn clip_id(defs: &Defs, clip: Rect) -> Option<String> {
    defs.clips
        .iter()
        .find(|(_, r)| *r == clip)
        .map(|(index, _)| format!("frame-clip-{index}"))
}

fn write_defs(w: &mut Writer, defs: &Defs) {
    if !defs.sticker_square
        && !defs.sticker_wide
        && !defs.arrow
        && !defs.circle_cap
        && defs.clips.is_empty()
    {
        return;
    }
    w.line("<defs>");
    w.depth += 1;

    // The sticky silhouette lives in the unit box and is scaled by each instance,
    // exactly as Miro's export does it. Four decimals: the path is scaled by up to
    // a few thousand, so two would show as a visible flat on the corner arc.
    for (id, present) in
        [("StickerType1", defs.sticker_square), ("StickerType2", defs.sticker_wide)]
    {
        if !present {
            continue;
        }
        let unit =
            Geometry::rounded(Rect::new(0.0, 0.0, 1.0, 1.0), crate::item::STICKY_CORNER_FRACTION)
                .to_path();
        w.line(&format!("<path id=\"{id}Path\" d=\"{}\"/>", path_data(&unit, 4)));
        // `fill="inherit"` is what carries each instance's own colour through the
        // `<use>`. Without it every sticky would be the definition's colour.
        w.line(&format!(
            "<g id=\"{id}\"><use xlink:href=\"#{id}Path\" fill=\"inherit\"/></g>"
        ));
    }

    if defs.arrow {
        // Tip at the origin, pointing east, one unit long. Instances rotate and
        // scale it, so the head grows with the connector's stroke width.
        w.line("<path id=\"LineHeadArrow1\" d=\"M0,0 L-1,-0.4 L-1,0.4 Z\"/>");
    }
    if defs.circle_cap {
        w.line("<circle id=\"LineHeadCircle1\" cx=\"0\" cy=\"0\" r=\"0.4\"/>");
    }

    for (index, clip) in &defs.clips {
        w.line(&format!(
            "<clipPath id=\"frame-clip-{index}\"><rect x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\"/></clipPath>",
            num(clip.x, w.precision),
            num(clip.y, w.precision),
            num(clip.width, w.precision),
            num(clip.height, w.precision),
        ));
    }

    w.depth -= 1;
    w.line("</defs>");
}

// ---------------------------------------------------------------------------
// Items
// ---------------------------------------------------------------------------

fn write_item(w: &mut Writer, scene: &Scene, defs: &Defs, placed: &Placed, options: &SvgOptions) {
    let item = &placed.item;

    // Wrappers, outermost first. Clip and transform are separate groups on purpose:
    // `clip-path` resolves in the user space the element establishes, so putting a
    // rotation on the same element would rotate the frame's clip rectangle too.
    let mut open = Vec::new();
    if let Some(link) = &item.link {
        open.push(format!("<a xlink:href=\"{}\">", escape_attr(link)));
    }
    if let Some(id) = placed.clip.and_then(|c| clip_id(defs, c)) {
        open.push(format!("<g clip-path=\"url(#{id})\">"));
    }
    let rotation = (item.rotation != 0.0).then(|| {
        let c = item.geometry.bounds().centre();
        format!(
            " transform=\"rotate({} {} {})\"",
            num(item.rotation, w.precision),
            num(c.x, w.precision),
            num(c.y, w.precision)
        )
    });
    let group_opacity = opacity_attr("opacity", item.style.opacity);
    if rotation.is_some() || !group_opacity.is_empty() {
        open.push(format!(
            "<g{}{}>",
            rotation.as_deref().unwrap_or_default(),
            group_opacity
        ));
    }

    let closers: Vec<&str> = open
        .iter()
        .map(|tag| if tag.starts_with("<a") { "</a>" } else { "</g>" })
        .rev()
        .collect();
    for tag in &open {
        w.line(tag);
        w.depth += 1;
    }

    write_body(w, scene, placed, options);

    for tag in closers {
        w.depth -= 1;
        w.line(tag);
    }
}

fn write_body(w: &mut Writer, scene: &Scene, placed: &Placed, options: &SvgOptions) {
    let item = &placed.item;
    let bounds = item.geometry.bounds();

    match &item.kind {
        Kind::Frame { order, .. } => {
            w.line(&format!("<g data-frame=\"true\" data-frame-order=\"{order}\">"));
            w.depth += 1;
            w.line(&format!(
                "<rect class=\"frame-background\" {}{}/>",
                rect_attrs(bounds, 0.0, w.precision),
                paint_attrs(&item.style, w.precision)
            ));
            if let Some(name) = item.name.as_ref().filter(|_| options.frame_titles) {
                // Miro draws a frame's name above its top-left corner, outside the
                // box, so it is never clipped by the frame's own contents.
                let font = crate::text::FontSpec::default();
                w.line(&format!(
                    "<text class=\"frame-title\" x=\"{}\" y=\"{}\" font-family=\"{}\" font-size=\"{}\" fill=\"#1a1d1f\">{}</text>",
                    num(bounds.x, w.precision),
                    num(bounds.y - font.size * CAPTION_BASELINE, w.precision),
                    escape_attr(&font.family),
                    num(font.size, w.precision),
                    escape_text(name)
                ));
            }
            w.depth -= 1;
            w.line("</g>");
        }

        Kind::Sticky => {
            let id = sticker_id(bounds);
            let fill = item.style.fill.unwrap_or(Color::WHITE);
            w.line(&format!(
                "<use xlink:href=\"#{id}\" transform=\"translate({} {}) scale({} {})\" fill=\"{}\"{}/>",
                num(bounds.x, w.precision),
                num(bounds.y, w.precision),
                num(bounds.width, w.precision),
                num(bounds.height, w.precision),
                fill.to_hex(),
                opacity_attr("fill-opacity", fill.opacity()),
            ));
            write_text(w, placed);
        }

        Kind::Shape { name } => {
            let class = format!("shape-element shape-element-{name}");
            match &item.geometry {
                Geometry::Rect { rect, corner_radius } => w.line(&format!(
                    "<rect class=\"{class}\" {}{}/>",
                    rect_attrs(*rect, *corner_radius, w.precision),
                    paint_attrs(&item.style, w.precision)
                )),
                Geometry::Ellipse { rect } => w.line(&format!(
                    "<ellipse class=\"{class}\" cx=\"{}\" cy=\"{}\" rx=\"{}\" ry=\"{}\"{}/>",
                    num(rect.centre().x, w.precision),
                    num(rect.centre().y, w.precision),
                    num(rect.width / 2.0, w.precision),
                    num(rect.height / 2.0, w.precision),
                    paint_attrs(&item.style, w.precision)
                )),
                // A straight-edged silhouette is written as `<polygon>`: shorter
                // than the equivalent `d`, and — because Miro's SVG reader tells ink
                // from decoration by the length of a `<path d>` — it also keeps a
                // large polygonal shape from reading back as an ink stroke.
                Geometry::Path(path) => match path.as_polygon() {
                    Some(points) => w.line(&format!(
                        "<polygon class=\"{class}\" points=\"{}\"{}/>",
                        points_data(&points, w.precision),
                        paint_attrs(&item.style, w.precision)
                    )),
                    None => w.line(&format!(
                        "<path class=\"{class}\" d=\"{}\"{}/>",
                        path_data(path, w.precision),
                        paint_attrs(&item.style, w.precision)
                    )),
                },
            }
            write_text(w, placed);
        }

        Kind::Ink => {
            // Ink is stroked, never filled: an open freehand stroke filled under the
            // non-zero rule would blot in every loop the user drew.
            w.line(&format!(
                "<path class=\"ink\" d=\"{}\" fill=\"none\"{}/>",
                path_data(&item.geometry.to_path(), w.precision),
                stroke_attrs(&item.style, w.precision)
            ));
        }

        Kind::Connector { start, end } => {
            let path = item.geometry.to_path();
            w.line("<g class=\"connector\">");
            w.depth += 1;
            w.line(&format!(
                "<path d=\"{}\" fill=\"none\"{}/>",
                path_data(&path, w.precision),
                stroke_attrs(&item.style, w.precision)
            ));
            write_end_caps(w, &path, *start, *end, &item.style);
            w.depth -= 1;
            w.line("</g>");
            write_text(w, placed);
        }

        Kind::Image => {
            write_image(w, scene, placed, options);
            write_text(w, placed);
        }

        Kind::Card | Kind::LinkPreview | Kind::Embed => {
            let class = match item.kind {
                // The trailing token matters: the reader distinguishes the outer
                // `preview-widget X` container from inner `preview-content_*` parts
                // by the space, so a bare `preview-widget` would not be counted.
                Kind::LinkPreview => "preview-widget preview-widget_default",
                Kind::Embed => "embed-widget",
                _ => "card-widget",
            };
            w.line(&format!("<g class=\"{class}\">"));
            w.depth += 1;
            w.line(&format!(
                "<rect class=\"card-background\" {}{}/>",
                rect_attrs(bounds, corner_radius_of(&item.geometry), w.precision),
                paint_attrs(&item.style, w.precision)
            ));
            if item.image.is_some() {
                write_image(w, scene, placed, options);
            }
            w.depth -= 1;
            w.line("</g>");
            write_text(w, placed);
        }

        Kind::Text => write_text(w, placed),
    }
}

fn corner_radius_of(geometry: &Geometry) -> f64 {
    match geometry {
        Geometry::Rect { corner_radius, .. } => *corner_radius,
        _ => 0.0,
    }
}

fn write_image(w: &mut Writer, scene: &Scene, placed: &Placed, options: &SvgOptions) {
    let bounds = placed.item.geometry.bounds();
    let data = placed.item.image.as_ref().and_then(|r| scene.image(r));
    match data.filter(|_| options.embed_images).and_then(data_uri) {
        Some(uri) => w.line(&format!(
            // `preserveAspectRatio="none"` because an item's rect is already the
            // crop: `docs/features/README.md` §1 makes cropping a UV rect, so the
            // stored image has been trimmed to exactly this box.
            "<image {} preserveAspectRatio=\"none\" href=\"{uri}\"/>",
            rect_attrs(bounds, 0.0, w.precision)
        )),
        None => {
            let reason = if placed.item.image.is_none() {
                "no-image"
            } else if options.embed_images {
                "unresolved"
            } else {
                "not-embedded"
            };
            w.line(&format!(
                "<rect class=\"image-placeholder\" data-reason=\"{reason}\" {} fill=\"#e3e6e8\"/>",
                rect_attrs(bounds, 0.0, w.precision)
            ));
        }
    }
}

fn data_uri(data: &ImageData) -> Option<String> {
    let (media_type, bytes) = match data {
        ImageData::Encoded { media_type, bytes } => (media_type.clone(), bytes.clone()),
        // Raw pixels have no wire format, so they are encoded once here rather than
        // by every caller that happens to have decoded an image already.
        ImageData::Rgba8 { width, height, pixels } => {
            ("image/png".to_string(), crate::raster::encode_rgba_png(*width, *height, pixels).ok()?)
        }
    };
    let payload = base64::engine::general_purpose::STANDARD.encode(&bytes);
    Some(format!("data:{media_type};base64,{payload}"))
}

fn write_end_caps(w: &mut Writer, path: &Path, start: EndCap, end: EndCap, style: &Style) {
    let Some((head, tail)) = path.terminals() else { return };
    let Some(stroke) = style.effective_stroke() else { return };
    // Heads scale with the line: a 1px connector with a 20px arrowhead reads as an
    // arrow that happens to have a line attached.
    let size = (stroke.width * 5.0).max(1.0);

    for (terminal, cap) in [(head, start), (tail, end)] {
        let id = match cap {
            EndCap::None => continue,
            EndCap::Arrow => "LineHeadArrow1",
            EndCap::Circle => "LineHeadCircle1",
        };
        w.line(&format!(
            "<use xlink:href=\"#{id}\" transform=\"translate({} {}) rotate({}) scale({})\" fill=\"{}\"/>",
            num(terminal.point.x, w.precision),
            num(terminal.point.y, w.precision),
            num(terminal.direction_deg, w.precision),
            num(size, w.precision),
            stroke.color.to_hex(),
        ));
    }
}

// ---------------------------------------------------------------------------
// Text
// ---------------------------------------------------------------------------

/// One `<text>` per line, which is how Miro's own export is structured and what
/// makes a line count comparable between the two.
fn write_text(w: &mut Writer, placed: &Placed) {
    let Some(block) = &placed.item.text else { return };
    if block.is_empty() {
        return;
    }
    let box_ = placed.item.text_box();
    let offset = block.valign_offset(box_.height);
    let (anchor, x) = match block.align {
        Align::Left => ("start", box_.x),
        Align::Center => ("middle", box_.centre().x),
        Align::Right => ("end", box_.right()),
    };

    for line in &block.lines {
        if line.is_blank() {
            continue;
        }
        let y = box_.y + offset + line.baseline;
        let common = line.spans.first().expect("a non-blank line has spans");
        let mut open = format!(
            "<text x=\"{}\" y=\"{}\" text-anchor=\"{anchor}\"{}>",
            num(x, w.precision),
            num(y, w.precision),
            font_attrs(common, w.precision)
        );
        if line.spans.len() == 1 {
            let _ = write!(open, "{}</text>", escape_text(&common.text));
            w.line(&open);
            continue;
        }
        w.line(&open);
        w.depth += 1;
        for span in &line.spans {
            if span.text.is_empty() {
                continue;
            }
            let tspan =
                format!("<tspan{}>{}</tspan>", font_attrs(span, w.precision), escape_text(&span.text));
            match &span.link {
                Some(link) => {
                    w.line(&format!("<a xlink:href=\"{}\">{tspan}</a>", escape_attr(link)))
                }
                None => w.line(&tspan),
            }
        }
        w.depth -= 1;
        w.line("</text>");
    }
}

fn font_attrs(span: &Span, precision: u8) -> String {
    let mut out = format!(
        " font-family=\"{}\" font-size=\"{}\" fill=\"{}\"",
        escape_attr(&span.font.family),
        num(span.font.size, precision),
        span.color.to_hex()
    );
    out.push_str(&opacity_attr("fill-opacity", span.color.opacity()));
    if span.font.weight != 400 {
        let _ = write!(out, " font-weight=\"{}\"", span.font.weight);
    }
    if span.font.italic {
        out.push_str(" font-style=\"italic\"");
    }
    match (span.underline, span.strikethrough) {
        (true, true) => out.push_str(" text-decoration=\"underline line-through\""),
        (true, false) => out.push_str(" text-decoration=\"underline\""),
        (false, true) => out.push_str(" text-decoration=\"line-through\""),
        (false, false) => {}
    }
    out
}

// ---------------------------------------------------------------------------
// Attribute helpers
// ---------------------------------------------------------------------------

fn rect_attrs(rect: Rect, corner_radius: f64, precision: u8) -> String {
    let mut out = format!(
        "x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\"",
        num(rect.x, precision),
        num(rect.y, precision),
        num(rect.width, precision),
        num(rect.height, precision)
    );
    let r = corner_radius.min(rect.width / 2.0).min(rect.height / 2.0);
    if r > 0.0 {
        let _ = write!(out, " rx=\"{}\"", num(r, precision));
    }
    out
}

fn paint_attrs(style: &Style, precision: u8) -> String {
    let mut out = match style.effective_fill() {
        Some(fill) => {
            format!(" fill=\"{}\"{}", fill.to_hex(), opacity_attr("fill-opacity", fill.opacity()))
        }
        None => " fill=\"none\"".to_string(),
    };
    out.push_str(&stroke_attrs(style, precision));
    out
}

fn stroke_attrs(style: &Style, precision: u8) -> String {
    let Some(stroke) = style.effective_stroke() else { return String::new() };
    let mut out = format!(
        " stroke=\"{}\" stroke-width=\"{}\"",
        stroke.color.to_hex(),
        num(stroke.width, precision)
    );
    out.push_str(&opacity_attr("stroke-opacity", stroke.color.opacity()));
    if stroke.cap != crate::style::LineCap::Butt {
        let _ = write!(out, " stroke-linecap=\"{}\"", stroke.cap.svg());
    }
    if stroke.join != crate::style::LineJoin::Miter {
        let _ = write!(out, " stroke-linejoin=\"{}\"", stroke.join.svg());
    }
    if let Some(pattern) = stroke.dash.pattern(stroke.width) {
        let dashes: Vec<String> = pattern.iter().map(|v| num(*v, precision)).collect();
        let _ = write!(out, " stroke-dasharray=\"{}\"", dashes.join(" "));
    }
    out
}

/// An opacity attribute, omitted when it is fully opaque. Writing `="1"` on every
/// element is a measurable share of a large export's bytes and says nothing.
fn opacity_attr(name: &str, value: f32) -> String {
    if value >= 1.0 { String::new() } else { format!(" {name}=\"{}\"", num(value as f64, 3)) }
}

/// The `d` attribute for a path.
pub(crate) fn path_data(path: &Path, precision: u8) -> String {
    let mut d = String::new();
    for sub in &path.subpaths {
        if sub.segments.is_empty() {
            continue;
        }
        if !d.is_empty() {
            d.push(' ');
        }
        d.push('M');
        write_point(&mut d, sub.start, precision);
        for segment in &sub.segments {
            match *segment {
                Segment::Line { to } => {
                    d.push_str(" L");
                    write_point(&mut d, to, precision);
                }
                Segment::Quadratic { ctrl, to } => {
                    d.push_str(" Q");
                    write_point(&mut d, ctrl, precision);
                    d.push(' ');
                    write_point(&mut d, to, precision);
                }
                Segment::Cubic { ctrl1, ctrl2, to } => {
                    d.push_str(" C");
                    write_point(&mut d, ctrl1, precision);
                    d.push(' ');
                    write_point(&mut d, ctrl2, precision);
                    d.push(' ');
                    write_point(&mut d, to, precision);
                }
            }
        }
        if sub.closed {
            d.push_str(" Z");
        }
    }
    d
}

/// The `points` attribute for a `<polygon>`.
fn points_data(points: &[Point], precision: u8) -> String {
    let mut out = String::new();
    for point in points {
        if !out.is_empty() {
            out.push(' ');
        }
        write_point(&mut out, *point, precision);
    }
    out
}

fn write_point(out: &mut String, p: Point, precision: u8) {
    write_num(out, p.x, precision);
    out.push(',');
    write_num(out, p.y, precision);
}

/// Escapes character data. `>` is escaped as well as `<` and `&` — it only *has* to
/// be inside `]]>`, but a reader that sees `]]>` in text is a reader that stops
/// early, and the cost is one entity.
fn escape_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            // XML 1.0 forbids these outright, and Miro's rich text has been seen to
            // carry stray control characters from pasted content.
            c if (c as u32) < 0x20 && c != '\t' && c != '\n' && c != '\r' => {}
            c => out.push(c),
        }
    }
    out
}

fn escape_attr(text: &str) -> String {
    escape_text(text).replace('"', "&quot;").replace('\'', "&apos;")
}

// ---------------------------------------------------------------------------

struct Writer<'a> {
    out: String,
    depth: usize,
    precision: u8,
    options: &'a SvgOptions,
}

impl<'a> Writer<'a> {
    fn new(options: &'a SvgOptions) -> Self {
        Self { out: String::with_capacity(64 * 1024), depth: 0, precision: options.precision, options }
    }

    fn raw(&mut self, text: &str) {
        self.out.push_str(text);
        self.out.push('\n');
    }

    fn line(&mut self, text: &str) {
        if self.options.indent {
            for _ in 0..self.depth {
                self.out.push_str("  ");
            }
        }
        self.out.push_str(text);
        self.out.push('\n');
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::geom::{SubPath, pt};
    use crate::item::{Item, ImageRef};
    use crate::scene::Scene;
    use crate::source::{Scope, Snapshot};
    use crate::style::Stroke;
    use crate::text::{FontSpec, TextBlock};

    fn render(items: Vec<Item>) -> String {
        let scene = Scene::collect(&Snapshot::new(items), &Scope::Board).expect("content");
        write(&scene, &SvgOptions::default()).expect("a scene renders")
    }

    fn sticky(id: u64, rect: Rect, color: &str) -> Item {
        Item::new(id, Kind::Sticky, Geometry::rect(rect))
            .with_style(Style::filled(Color::from_hex(color).unwrap()))
    }

    #[test]
    fn the_root_carries_size_and_view_box() {
        let svg = render(vec![sticky(1, Rect::new(10.0, 20.0, 100.0, 100.0), "#fff79e")]);
        assert!(svg.contains("width=\"100px\" height=\"100px\""), "{svg}");
        assert!(svg.contains("viewBox=\"10 20 100 100\""), "{svg}");
    }

    /// The definition must not look like an instance — this is the trap
    /// `vellum_import::svg` documents, and the exporter has to stay on the right
    /// side of it.
    #[test]
    fn the_sticky_definition_references_the_path_suffix_not_the_symbol() {
        let svg = render(vec![sticky(1, Rect::new(0.0, 0.0, 100.0, 100.0), "#fff79e")]);
        assert!(svg.contains("<g id=\"StickerType1\"><use xlink:href=\"#StickerType1Path\""), "{svg}");
        assert_eq!(svg.matches("xlink:href=\"#StickerType1\"").count(), 1, "{svg}");
    }

    #[test]
    fn sticky_aspect_chooses_between_the_two_symbols() {
        let svg = render(vec![
            sticky(1, Rect::new(0.0, 0.0, 100.0, 100.0), "#fff79e"),
            sticky(2, Rect::new(200.0, 0.0, 300.0, 100.0), "#ff9e9e"),
        ]);
        assert!(svg.contains("xlink:href=\"#StickerType1\""), "{svg}");
        assert!(svg.contains("xlink:href=\"#StickerType2\""), "{svg}");
    }

    #[test]
    fn a_shape_keeps_its_type_in_its_class() {
        let svg = render(vec![
            Item::new(
                1,
                Kind::Shape { name: "cloud" },
                Geometry::Path(Path::from_outline(
                    &vellum_shapes::Shape::Cloud.outline(1.0),
                    Rect::new(0.0, 0.0, 100.0, 100.0),
                )),
            )
            .with_style(Style::filled(Color::WHITE)),
        ]);
        assert!(svg.contains("class=\"shape-element shape-element-cloud\""), "{svg}");
    }

    #[test]
    fn a_rectangular_shape_is_a_rect_element_not_a_path() {
        let svg = render(vec![
            Item::new(
                1,
                Kind::Shape { name: "rectangle" },
                Geometry::rounded(Rect::new(0.0, 0.0, 40.0, 20.0), 4.0),
            )
            .with_style(Style::filled(Color::WHITE)),
        ]);
        assert!(svg.contains("<rect class=\"shape-element shape-element-rectangle\""), "{svg}");
        assert!(svg.contains("rx=\"4\""), "{svg}");
        assert!(!svg.contains("<path class=\"shape-element"), "{svg}");
    }

    /// Straight-edged silhouettes go out as `<polygon>` — shorter, and out of reach
    /// of the reader's path-length ink heuristic however large the shape is.
    #[test]
    fn a_straight_edged_shape_is_a_polygon_not_a_path() {
        let svg = render(vec![
            Item::new(
                1,
                Kind::Shape { name: "diamond" },
                Geometry::Path(Path::from_outline(
                    &vellum_shapes::Shape::Diamond.outline(1.0),
                    Rect::new(0.0, 0.0, 100.0, 100.0),
                )),
            )
            .with_style(Style::filled(Color::WHITE)),
        ]);
        assert!(svg.contains("<polygon class=\"shape-element shape-element-diamond\""), "{svg}");
        assert!(svg.contains("points=\"50,0 100,50 50,100 0,50\""), "{svg}");
        assert!(!svg.contains("<path class=\"shape-element"), "{svg}");
    }

    /// A caption drawn above a frame must be inside the view box, or every frame
    /// name is cropped off the export.
    #[test]
    fn the_view_box_makes_room_for_frame_captions() {
        let items = vec![
            Item::new(1, Kind::frame(0), Geometry::rect(Rect::new(0.0, 100.0, 200.0, 200.0)))
                .with_name("Wiring"),
        ];
        let scene = Scene::collect(&Snapshot::new(items), &Scope::Board).unwrap();
        let view = view_box(&scene, &SvgOptions::default());
        assert!(view.y < 100.0, "the caption strip is missing: {view:?}");

        // The caption's baseline is inside it.
        let svg = write(&scene, &SvgOptions::default()).unwrap();
        let y: f64 = svg
            .split("class=\"frame-title\" x=\"0\" y=\"")
            .nth(1)
            .and_then(|rest| rest.split('"').next())
            .and_then(|v| v.parse().ok())
            .expect("a caption");
        assert!(y > view.y && y < 100.0, "baseline {y} outside view {view:?}");

        // And no room is wasted when captions are off.
        let bare = view_box(&scene, &SvgOptions { frame_titles: false, ..SvgOptions::default() });
        assert_eq!(bare, scene.bounds);
    }

    #[test]
    fn frame_titles_can_be_turned_off() {
        let items = vec![
            Item::new(1, Kind::frame(0), Geometry::rect(Rect::new(0.0, 0.0, 100.0, 100.0)))
                .with_name("Wiring"),
        ];
        let scene = Scene::collect(&Snapshot::new(items), &Scope::Board).unwrap();
        let options = SvgOptions { frame_titles: false, ..SvgOptions::default() };
        let svg = write(&scene, &options).unwrap();
        assert!(!svg.contains("Wiring"), "{svg}");
    }

    #[test]
    fn a_frame_is_marked_and_names_itself_above_its_box() {
        let svg = render(vec![
            Item::new(1, Kind::frame(3), Geometry::rect(Rect::new(0.0, 100.0, 200.0, 200.0)))
                .with_name("Coolant System"),
        ]);
        assert!(svg.contains("data-frame=\"true\""), "{svg}");
        assert!(svg.contains("data-frame-order=\"3\""), "{svg}");
        assert!(svg.contains(">Coolant System</text>"), "{svg}");
        // The title sits above y = 100, outside the frame.
        let y = svg.split("class=\"frame-title\" x=\"0\" y=\"").nth(1).unwrap();
        let y: f64 = y.split('"').next().unwrap().parse().unwrap();
        assert!(y < 100.0, "the frame title must clear the frame's top edge, got {y}");
    }

    #[test]
    fn a_clipped_child_is_wrapped_in_the_frames_clip_path() {
        let items = vec![
            Item::new(1, Kind::frame(0), Geometry::rect(Rect::new(0.0, 0.0, 100.0, 100.0))),
            sticky(2, Rect::new(80.0, 10.0, 50.0, 50.0), "#fff79e").in_frame(crate::ItemId(1)),
            sticky(3, Rect::new(10.0, 10.0, 50.0, 50.0), "#fff79e").in_frame(crate::ItemId(1)),
        ];
        let svg = render(items);
        assert!(svg.contains("<clipPath id=\"frame-clip-"), "{svg}");
        assert_eq!(svg.matches("<clipPath").count(), 1, "one clip serves the whole frame: {svg}");
        assert_eq!(svg.matches("clip-path=\"url(#frame-clip-").count(), 2, "{svg}");
    }

    #[test]
    fn rotation_and_clip_are_separate_groups() {
        let items = vec![
            Item::new(1, Kind::frame(0), Geometry::rect(Rect::new(0.0, 0.0, 100.0, 100.0))),
            sticky(2, Rect::new(10.0, 10.0, 50.0, 50.0), "#fff79e")
                .in_frame(crate::ItemId(1))
                .with_rotation(30.0),
        ];
        let svg = render(items);
        // The clip group opens before the rotation group, so the clip rectangle is
        // never rotated with its contents.
        let clip_at = svg.find("clip-path=\"url(#").expect("a clip");
        let rotate_at = svg.find("transform=\"rotate(").expect("a rotation");
        assert!(clip_at < rotate_at, "{svg}");
    }

    #[test]
    fn ink_is_stroked_and_never_filled() {
        let path = Path::new(vec![
            SubPath::polyline(&[pt(0.0, 0.0), pt(10.0, 10.0), pt(20.0, 0.0)], false).unwrap(),
        ]);
        let svg = render(vec![
            Item::new(1, Kind::Ink, Geometry::Path(path))
                .with_style(Style::NONE.with_stroke(Stroke::ink(Color::BLACK, 3.0))),
        ]);
        assert!(svg.contains("class=\"ink\""), "{svg}");
        assert!(svg.contains("fill=\"none\""), "{svg}");
        assert!(svg.contains("stroke-linecap=\"round\""), "{svg}");
    }

    #[test]
    fn a_connector_arrowhead_points_along_the_line() {
        let path = Path::new(vec![
            SubPath::polyline(&[pt(0.0, 0.0), pt(100.0, 0.0)], false).unwrap(),
        ]);
        let svg = render(vec![
            Item::new(
                1,
                Kind::Connector { start: EndCap::None, end: EndCap::Arrow },
                Geometry::Path(path),
            )
            .with_style(Style::NONE.with_stroke(Stroke::new(Color::BLACK, 2.0))),
        ]);
        assert_eq!(svg.matches("xlink:href=\"#LineHeadArrow1\"").count(), 1, "{svg}");
        assert!(svg.contains("translate(100 0) rotate(0)"), "{svg}");
    }

    #[test]
    fn both_end_caps_are_drawn_when_both_are_set() {
        let path = Path::new(vec![
            SubPath::polyline(&[pt(0.0, 0.0), pt(100.0, 0.0)], false).unwrap(),
        ]);
        let svg = render(vec![
            Item::new(
                1,
                Kind::Connector { start: EndCap::Arrow, end: EndCap::Arrow },
                Geometry::Path(path),
            )
            .with_style(Style::NONE.with_stroke(Stroke::new(Color::BLACK, 2.0))),
        ]);
        assert_eq!(svg.matches("xlink:href=\"#LineHeadArrow1\"").count(), 2, "{svg}");
        assert!(svg.contains("rotate(180)"), "the start cap faces back down the line: {svg}");
    }

    #[test]
    fn text_is_one_element_per_line_with_the_requested_anchor() {
        let block = TextBlock::plain("one\ntwo", FontSpec::new("Noto Sans", 14.0), Color::BLACK)
            .with_align(Align::Center);
        let svg = render(vec![
            Item::new(1, Kind::Text, Geometry::rect(Rect::new(0.0, 0.0, 200.0, 100.0)))
                .with_text(block),
        ]);
        assert_eq!(svg.matches("<text ").count(), 2, "{svg}");
        assert!(svg.contains("text-anchor=\"middle\""), "{svg}");
        assert!(svg.contains("font-family=\"Noto Sans\""), "{svg}");
        assert!(svg.contains(">one</text>"), "{svg}");
    }

    #[test]
    fn blank_lines_produce_no_elements() {
        let block = TextBlock::plain("a\n\n\nb", FontSpec::default(), Color::BLACK);
        let svg = render(vec![
            Item::new(1, Kind::Text, Geometry::rect(Rect::new(0.0, 0.0, 200.0, 100.0)))
                .with_text(block),
        ]);
        assert_eq!(svg.matches("<text ").count(), 2, "{svg}");
    }

    #[test]
    fn mixed_spans_become_tspans_inside_one_line() {
        let line = crate::text::TextLine::new(
            20.0,
            vec![
                Span::new("plain ", FontSpec::new("Inter", 14.0), Color::BLACK),
                Span::new("bold", FontSpec::new("Inter", 14.0).bold(), Color::BLACK),
            ],
        );
        let svg = render(vec![
            Item::new(1, Kind::Text, Geometry::rect(Rect::new(0.0, 0.0, 200.0, 100.0)))
                .with_text(TextBlock::new(vec![line])),
        ]);
        assert_eq!(svg.matches("<text ").count(), 1, "{svg}");
        assert_eq!(svg.matches("<tspan").count(), 2, "{svg}");
        assert!(svg.contains("font-weight=\"700\""), "{svg}");
    }

    #[test]
    fn text_and_attributes_are_escaped() {
        let block = TextBlock::plain("a < b & \"c\"", FontSpec::default(), Color::BLACK);
        let svg = render(vec![
            Item::new(1, Kind::Text, Geometry::rect(Rect::new(0.0, 0.0, 200.0, 100.0)))
                .with_text(block)
                .with_link("https://example.com/?a=1&b=\"2\""),
        ]);
        assert!(svg.contains("a &lt; b &amp; \"c\""), "{svg}");
        assert!(svg.contains("?a=1&amp;b=&quot;2&quot;"), "{svg}");
    }

    #[test]
    fn control_characters_are_dropped_rather_than_written() {
        let block = TextBlock::plain("a\u{0}b\u{7}", FontSpec::default(), Color::BLACK);
        let svg = render(vec![
            Item::new(1, Kind::Text, Geometry::rect(Rect::new(0.0, 0.0, 200.0, 100.0)))
                .with_text(block),
        ]);
        assert!(svg.contains(">ab</text>"), "{svg}");
    }

    #[test]
    fn an_image_is_embedded_as_a_data_uri() {
        let source = Snapshot::new(vec![
            Item::new(1, Kind::Image, Geometry::rect(Rect::new(0.0, 0.0, 10.0, 10.0)))
                .with_image(ImageRef::new("k")),
        ])
        .with_image("k", ImageData::encoded("image/jpeg", vec![1, 2, 3, 4]));
        let scene = Scene::collect(&source, &Scope::Board).unwrap();
        let svg = write(&scene, &SvgOptions::default()).unwrap();
        assert!(svg.contains("href=\"data:image/jpeg;base64,AQIDBA==\""), "{svg}");
    }

    #[test]
    fn an_unresolvable_image_leaves_a_placeholder_that_says_why() {
        let svg = render(vec![
            Item::new(1, Kind::Image, Geometry::rect(Rect::new(0.0, 0.0, 10.0, 10.0)))
                .with_image(ImageRef::new("missing")),
        ]);
        assert!(svg.contains("class=\"image-placeholder\" data-reason=\"unresolved\""), "{svg}");
    }

    #[test]
    fn images_can_be_left_out_entirely() {
        let source = Snapshot::new(vec![
            Item::new(1, Kind::Image, Geometry::rect(Rect::new(0.0, 0.0, 10.0, 10.0)))
                .with_image(ImageRef::new("k")),
        ])
        .with_image("k", ImageData::encoded("image/png", vec![1, 2, 3, 4]));
        let scene = Scene::collect(&source, &Scope::Board).unwrap();
        let svg =
            write(&scene, &SvgOptions { embed_images: false, ..SvgOptions::default() }).unwrap();
        assert!(!svg.contains("base64"), "{svg}");
        assert!(svg.contains("data-reason=\"not-embedded\""), "{svg}");
    }

    #[test]
    fn a_link_preview_carries_the_class_the_reader_looks_for() {
        let svg = render(vec![
            Item::new(1, Kind::LinkPreview, Geometry::rect(Rect::new(0.0, 0.0, 100.0, 60.0)))
                .with_style(Style::filled(Color::WHITE))
                .with_link("https://example.com/products/kv16"),
        ]);
        assert!(svg.contains("class=\"preview-widget preview-widget_default\""), "{svg}");
        assert!(svg.contains("<a xlink:href=\"https://example.com/products/kv16\">"), "{svg}");
    }

    #[test]
    fn opacity_of_one_is_never_written() {
        let svg = render(vec![sticky(1, Rect::new(0.0, 0.0, 10.0, 10.0), "#fff79e")]);
        assert!(!svg.contains("opacity=\"1\""), "{svg}");
    }

    #[test]
    fn item_opacity_becomes_a_group_opacity() {
        let svg = render(vec![
            sticky(1, Rect::new(0.0, 0.0, 10.0, 10.0), "#fff79e")
                .with_style(Style::filled(Color::WHITE).with_opacity(0.4)),
        ]);
        assert!(svg.contains("<g opacity=\"0.4\">"), "{svg}");
    }

    #[test]
    fn dashes_scale_with_the_stroke_width() {
        let svg = render(vec![
            Item::new(1, Kind::Shape { name: "rectangle" }, Geometry::rect(Rect::new(0.0, 0.0, 10.0, 10.0)))
                .with_style(
                    Style::NONE
                        .with_stroke(Stroke::new(Color::BLACK, 2.0).with_dash(crate::style::Dash::Dashed)),
                ),
        ]);
        assert!(svg.contains("stroke-dasharray=\"8 6\""), "{svg}");
    }

    #[test]
    fn path_data_is_trimmed_to_the_requested_precision() {
        let path = Path::new(vec![
            SubPath::polyline(&[pt(0.123_456, 1.0), pt(2.0, 3.0)], true).unwrap(),
        ]);
        assert_eq!(path_data(&path, 2), "M0.12,1 L2,3 Z");
        assert_eq!(path_data(&path, 4), "M0.1235,1 L2,3 Z");
    }

    #[test]
    fn an_empty_scene_is_refused() {
        let source = Snapshot::default();
        assert!(Scene::collect(&source, &Scope::Board).is_err());
    }
}
