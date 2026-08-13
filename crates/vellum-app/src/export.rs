//! Adapting a `vellum-doc` board onto `vellum-export`'s scene model.
//!
//! `vellum-export` writes SVG, PDF, CSV and PNG from a scene of its own — a flat list
//! of [`vellum_export::Item`]s with explicit geometry, style and text. It models that
//! separately on purpose: an exporter that read the CRDT directly would have to be
//! rebuilt every time the document grew a variant, and it could not run off the UI
//! thread. Nothing adapted the two, so **PDF and SVG export were a menu row that
//! reported a gap** while the whole writer sat finished behind it.
//!
//! This is the adapter, and it is the only thing that has to know both models.
//!
//! # Text is shaped, and what that does and does not mean
//!
//! [`BoardExport::shape_text`] runs the same `vellum-text` engine the canvas draws with
//! over every text-bearing item, and turns the result into [`TextBlock`] lines. It used
//! to use `TextBlock::plain`, which splits on `\n` and spaces the baselines — so **a
//! wrapped paragraph exported as one long line running off the side of the page**, and an
//! auto-fitted sticky exported at 14pt whatever size it was drawn at.
//!
//! What shaping is used for is *where the lines break, how tall they are, and what size
//! an auto-fitted block resolved to*. What is **not** emitted is glyph positions: each
//! line goes out as its own source substring, and the SVG or PDF viewer shapes it again
//! with its own copy of the font. That is the right division — an SVG full of per-glyph
//! `x` offsets is unselectable, unsearchable and enormous — but it means the *within-line*
//! metrics are the viewer's, so a line can come out a little wider or narrower there than
//! it is on the board. Line breaks, which are what actually go wrong, are ours.

use std::collections::HashMap;

use vellum_doc::{ItemKind, Placement, Style as DocStyle};
use vellum_export::{
    Color, FontSpec, Geometry, ImageData, ImageRef, Item, ItemId, Kind, Path, Point, Rect, Stroke,
    Style, SubPath, TextBlock, ZOrder,
};
use vellum_store::BlobStore;

use crate::project::{Projected, Projection};

/// A board, ready for [`vellum_export::Scene::collect`].
pub struct BoardExport<'a> {
    projection: &'a Projection,
    blobs: &'a BlobStore,
    title: Option<String>,
    /// Decoded once per export and held, because `BoardSource::image` takes `&self`.
    images: HashMap<String, ImageData>,
    /// Shaped text, for the same reason: `BoardSource::items` takes `&self` and shaping
    /// needs `&mut TextEngine`. Filled by [`Self::shape_text`]; an item missing from here
    /// falls back to the unshaped split, which is what a caller with no font stack gets.
    text: HashMap<vellum_scene::ItemId, TextBlock>,
}

impl<'a> BoardExport<'a> {
    pub fn new(projection: &'a Projection, blobs: &'a BlobStore, title: Option<String>) -> Self {
        Self {
            projection,
            blobs,
            title,
            images: HashMap::new(),
            text: HashMap::new(),
        }
    }

    /// Shapes every text-bearing item's words, so the export gets real line breaks and the
    /// size an auto-fitted block actually resolved to.
    ///
    /// Eager and up front, because `BoardSource::items` takes `&self` and shaping needs the
    /// engine mutably. An export is a discrete act, so paying for every item once here is
    /// cheaper than the per-frame caching the painter needs.
    ///
    /// A caller that skips this still gets output — `TextBlock::plain`'s unshaped split —
    /// which is what keeps this crate's tests and the CSV path working without fonts.
    pub fn shape_text(&mut self, engine: &mut vellum_text::TextEngine) {
        let items: Vec<(vellum_scene::ItemId, Placement, DocStyle, vellum_doc::StyledText)> = self
            .projection
            .iter()
            .filter_map(|(scene, projected)| {
                // A frame's title is drawn outside its box and becomes the item's `name`,
                // not its text — so it is not laid out to the frame's width, and shaping it
                // to one would be wrong rather than merely unnecessary.
                if matches!(projected.item.kind, ItemKind::Frame { .. }) {
                    return None;
                }
                let text = projected.item.kind.text()?;
                if text.is_empty() {
                    return None;
                }
                Some((
                    *scene,
                    projected.item.placement,
                    projected.item.style.clone(),
                    text.clone(),
                ))
            })
            .collect();

        for (scene, placement, style, text) in items {
            if let Some(block) = shaped_block(&text, &style, &placement, engine) {
                self.text.insert(scene, block);
            }
        }
    }

    /// Reads every image the board references into memory.
    ///
    /// Eager because the trait's `image` takes `&self` and cannot lazily populate a
    /// cache, and because an export is a discrete act rather than a per-frame cost.
    pub fn load_images(&mut self) {
        let referenced: Vec<String> = self
            .projection
            .iter()
            .filter_map(|(_, projected)| match &projected.item.kind {
                ItemKind::Image { asset_id, .. } if !asset_id.is_empty() => Some(asset_id.clone()),
                _ => None,
            })
            .collect();
        for asset in referenced {
            if self.images.contains_key(&asset) {
                continue;
            }
            let Ok(hash) = asset.parse() else { continue };
            match self.blobs.get(&hash) {
                Ok(Some(bytes)) => {
                    // The media type from the bytes themselves, since the blob store
                    // is content-addressed and holds no metadata. SVG needs it for the
                    // data URI's prefix and PDF for the filter it picks.
                    let media = image::guess_format(&bytes)
                        .map_or("application/octet-stream", |f| f.to_mime_type());
                    self.images.insert(asset, ImageData::encoded(media, bytes));
                }
                Ok(None) => log::warn!("exporting: asset {asset} is not in the blob store"),
                Err(error) => log::warn!("exporting asset {asset}: {error}"),
            }
        }
    }
}

impl vellum_export::BoardSource for BoardExport<'_> {
    fn items(&self) -> impl Iterator<Item = Item> + '_ {
        // A connector's targets are looked up through this, which is why `items` needs the
        // whole projection rather than one item at a time.
        let placements = move |id: vellum_doc::ItemId| self.projection.placement_of(id);
        self.projection.iter().filter_map(move |(scene, projected)| {
            convert(*scene, projected, self.text.get(scene), &placements)
        })
    }

    fn image(&self, image: &ImageRef) -> Option<ImageData> {
        self.images.get(&image.0).cloned()
    }

    fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }
}

/// The rectangle an item occupies, before rotation.
fn rect_of(placement: &Placement) -> Rect {
    let (w, h) = placement.scaled_size();
    Rect::new(placement.x - w / 2.0, placement.y - h / 2.0, w, h)
}

fn colour(c: vellum_doc::Color) -> Color {
    Color::rgba(c.r, c.g, c.b, c.a)
}

/// One document item as an export item, or `None` for a kind that draws nothing.
fn convert(
    scene: vellum_scene::ItemId,
    projected: &Projected,
    shaped: Option<&TextBlock>,
    placements: &dyn Fn(vellum_doc::ItemId) -> Option<Placement>,
) -> Option<Item> {
    let placement = &projected.item.placement;
    let rect = rect_of(placement);
    let style = &projected.item.style;
    let opacity = projected.opacity();

    let (kind, geometry, fill) = match &projected.item.kind {
        ItemKind::Sticky { background, .. } => (
            Kind::Sticky,
            Geometry::sticky(rect),
            Some(background.map_or(MIRO_YELLOW, colour)),
        ),
        ItemKind::Text { .. } => (Kind::Text, Geometry::rect(rect), None),
        ItemKind::Frame { order, .. } => (
            Kind::Frame {
                // Frames with no place in the deck sort after those that have one,
                // rather than all colliding at page zero.
                order: u32::try_from(order.unwrap_or(i64::from(u32::MAX))).unwrap_or(u32::MAX),
                clips: true,
            },
            Geometry::rect(rect),
            Some(style.fill.map_or(Color::WHITE, colour)),
        ),
        ItemKind::Shape { form, .. } => {
            let shape = crate::shapes::decode(form);
            let outline = shape.outline(aspect(rect));
            (
                Kind::Shape { name: shape.name() },
                Geometry::Path(Path::from_outline(&outline, rect)),
                Some(style.fill.map_or(Color::WHITE, colour)),
            )
        }
        ItemKind::Ink { points, .. } => {
            // Ink is a *stroked path*, not a filled silhouette: the document stores a
            // centreline and a width, which is exactly what `Stroke` expresses.
            let absolute: Vec<Point> = points
                .iter()
                .map(|p| {
                    let (x, y) = rotate_about(placement, p.x, p.y);
                    Point::new(x, y)
                })
                .collect();
            let sub = SubPath::polyline(&absolute, false)?;
            (Kind::Ink, Geometry::Path(Path::new(vec![sub])), None)
        }
        // A table exports as its outline. Its cells would need the laid-out grid, and
        // `Scene::collect` runs without a font stack — see the module docs on text.
        ItemKind::Table { .. } => (
            Kind::Card,
            Geometry::rect(rect),
            Some(style.fill.map_or(Color::WHITE, colour)),
        ),
        // A chart exports as its outline, for the same reason a table does: its marks
        // need a layout, and `Scene::collect` runs without a font stack to measure the
        // labels that layout depends on.
        ItemKind::Chart { .. } => (
            Kind::Card,
            Geometry::rect(rect),
            Some(style.fill.map_or(Color::WHITE, colour)),
        ),
        // And a mind map, for the same reason again — and more sharply, because a mind
        // map's *geometry* is derived from its shaped labels rather than merely
        // annotated by them: without a font stack there is no tidy layout to export at
        // all, not just no text on top of one.
        ItemKind::MindMap { .. } => (
            Kind::Card,
            Geometry::rect(rect),
            Some(style.fill.map_or(Color::WHITE, colour)),
        ),
        // A kanban board exports as its outline. Its columns and cards need the layout,
        // and every card's height in that layout is a shaped measurement.
        ItemKind::Kanban { .. } => (
            Kind::Card,
            Geometry::rect(rect),
            Some(style.fill.map_or(Color::WHITE, colour)),
        ),
        // The Agent Canvas kinds export as their card — the box, its fill and whatever text
        // this adapter was handed. A transcript is deliberately *not* exported: it is not
        // board content, it lives outside the document, and an SVG of an agent's scrollback
        // is not a picture of the board anybody wants.
        ItemKind::Agent { .. } | ItemKind::AgentNote { .. } | ItemKind::FileTree { .. } => (
            Kind::Card,
            Geometry::rect(rect),
            Some(style.fill.map_or(Color::WHITE, colour)),
        ),
        // A browser node exports as its poster, not its page: the page is live and this is
        // paper. With no engine running there is nothing to capture in any case.
        ItemKind::Browser { .. } => (
            Kind::Card,
            Geometry::rect(rect),
            Some(style.fill.map_or(Color::WHITE, colour)),
        ),
        ItemKind::Image { .. } => (Kind::Image, Geometry::rect(rect), None),
        ItemKind::LinkPreview { .. } => (Kind::LinkPreview, Geometry::rect(rect), None),
        ItemKind::Embed { .. } => (Kind::Embed, Geometry::rect(rect), None),
        ItemKind::Document { .. } => (Kind::Image, Geometry::rect(rect), None),
        // A connector's route is resolved from live bounds and never stored, so exporting
        // one means routing it here. That is now what happens — see `routed_path`. A route
        // that comes back with fewer than two points is a connector whose ends coincide, and
        // there is nothing to draw.
        ItemKind::Connector { start, end, .. } => {
            let sub = routed_path(projected, placements)?;
            (
                // The arrowheads travel too: `Kind::Connector` carries them, and all three
                // writers build the same triangle from `EndCap::path` — so an exported
                // connector's head matches the SVG oracle's `#LineHeadArrow1` rather than
                // being a shape of this adapter's own invention.
                Kind::Connector { start: end_cap(start.arrowhead), end: end_cap(end.arrowhead) },
                Geometry::Path(Path::new(vec![sub])),
                None,
            )
        }
        // A group is a container with no visual payload of its own — `ItemKind::Group` is
        // explicit about that — so it draws nothing here either.
        ItemKind::Group => return None,
    };

    let mut export_style = Style { fill, stroke: None, opacity };
    // A connector *is* its line, so its colour and thickness live on the kind exactly as an
    // ink stroke's do — and like ink, it has no fill.
    if let ItemKind::Connector { thickness, color, .. } = &projected.item.kind {
        export_style.fill = None;
        export_style.stroke = Some(Stroke::new(
            color.map_or(Color::BLACK, colour),
            thickness * placement.scale,
        ));
    }
    // An ink stroke's colour lives on the kind, everything else's outline on the style.
    if let ItemKind::Ink { color, thickness, .. } = &projected.item.kind {
        export_style.stroke =
            Some(Stroke::new(color.map_or(Color::BLACK, colour), thickness * placement.scale));
    } else if let Some(stroke) = style.stroke {
        export_style.stroke = Some(Stroke::new(colour(stroke), style.stroke_width.unwrap_or(1.0)));
    }

    let mut item = Item::new(scene, kind, geometry);
    item.z = ZOrder::from_index(u32::try_from(projected.z.max(0)).unwrap_or(u32::MAX));
    item.rotation = placement.rotation;
    item.style = export_style;
    item.frame = projected.parent.map(ItemId);

    if let ItemKind::Image { asset_id, .. } = &projected.item.kind
        && !asset_id.is_empty()
    {
        item.image = Some(ImageRef::new(asset_id.clone()));
    }

    // A frame's name is drawn *outside* its box and titles its PDF page, so it is the
    // item's `name` rather than its `text` — see `vellum_export::Item::name`.
    match &projected.item.kind {
        ItemKind::Frame { title, .. } => {
            let title = title.to_plain();
            if !title.trim().is_empty() {
                item.name = Some(title);
            }
        }
        kind => {
            if let Some(block) = shaped {
                // Shaped: real line breaks, real baselines, and the size an auto-fitted
                // block resolved to. See `BoardExport::shape_text`.
                item.text = Some(block.clone());
            } else if let Some(text) = kind.text() {
                // No font stack was offered. Better than nothing, and visibly worse: one
                // line per `\n` and no wrapping at all.
                let plain = text.to_plain();
                if !plain.trim().is_empty() {
                    let size = style.font_size.unwrap_or(DEFAULT_FONT_SIZE);
                    let family = style.font_family.clone().unwrap_or_else(|| "Inter".to_owned());
                    let ink = style.text_color.map_or(Color::BLACK, colour);
                    item.text = Some(TextBlock::plain(&plain, FontSpec::new(family, size), ink));
                }
            }
        }
    }

    Some(item)
}

/// A document arrowhead as the export model's cap.
///
/// The export model offers three caps and the document eight kinds, so this is lossy in one
/// direction only: every open or closed triangle becomes the filled arrow the SVG oracle
/// recognises, and a diamond or a bar — neither of which has ever been seen on a real Miro
/// board, per `docs/02-miro-formats.md` — becomes one too. Better a head of the wrong shape
/// than a connector that silently loses its direction.
const fn end_cap(arrow: vellum_doc::ArrowKind) -> vellum_export::EndCap {
    use vellum_doc::ArrowKind as A;
    use vellum_export::EndCap as E;
    match arrow {
        A::None => E::None,
        A::Circle => E::Circle,
        _ => E::Arrow,
    }
}

/// A connector's route, as an open sub-path in world coordinates.
///
/// Routed here rather than read from the document, because a connector's geometry is
/// **derived from live bounds every time it is asked for** — `crate::connector`'s header is
/// explicit that storing it would produce a connector that lies the moment either end moves.
/// So the export has to do the same resolution the painter does.
///
/// Curved routes are flattened, since neither SVG's nor PDF's path syntax is reached through
/// `vellum_export::SubPath`'s line-only model. `FLATTEN_TOLERANCE` is in board units and is
/// well under a printed dot at any sane page size.
///
/// Arrowheads are **not** exported: they are `ConnectorStyle` geometry that
/// `vellum-connect` builds as separate filled shapes, and threading those through as extra
/// items is a second piece of work. A connector therefore exports as its line.
fn routed_path(
    projected: &Projected,
    placements: &dyn Fn(vellum_doc::ItemId) -> Option<Placement>,
) -> Option<SubPath> {
    const FLATTEN_TOLERANCE: f64 = 0.25;
    let routed = crate::connector::route(
        &projected.item.kind,
        &projected.item.placement,
        placements,
        &vellum_connect::Router::default(),
        // No obstacles: avoidance needs every other item's box, and an orthogonal route that
        // dodged on screen but not on paper would be worse than one that never dodges.
        &[],
    )?;
    let points: Vec<Point> = routed
        .path
        .flatten(FLATTEN_TOLERANCE)
        .points
        .iter()
        .map(|p| Point::new(p.x, p.y))
        .collect();
    SubPath::polyline(&points, false)
}

/// One item's words as a laid-out [`TextBlock`], shaped by the real engine.
///
/// The interesting part is recovering the *source substring* of each visual line.
/// `vellum_text` reports glyphs, and the export writes characters — an SVG of per-glyph
/// offsets would be unselectable and enormous — so each line's text is reconstructed from
/// its glyphs' cluster ranges. Those ranges are **per paragraph**, so the paragraph's own
/// start offset has to be added back; that is the same trap `Layout::caret` documents, and
/// getting it wrong makes every line after the first a substring of the first.
fn shaped_block(
    text: &vellum_doc::StyledText,
    style: &DocStyle,
    placement: &Placement,
    engine: &mut vellum_text::TextEngine,
) -> Option<TextBlock> {
    let plain = text.to_plain();
    if plain.trim().is_empty() {
        return None;
    }
    let (width, height) = placement.scaled_size();
    #[expect(clippy::cast_possible_truncation, reason = "an item's box is screen-scale")]
    let fit = vellum_text::FitBox::new(width.max(1.0) as f32, height.max(1.0) as f32);
    let converted = crate::text::convert(text);
    let params = crate::text::params_for(style, Some(fit));
    // The size an auto-fitted block actually resolved to, not the nominal default. A
    // sticky's `fs: 0` means auto-fit — every sticky on the reference board — and exporting
    // those at 14pt was the second half of the old defect.
    #[expect(clippy::cast_possible_truncation, reason = "a font size is screen-scale")]
    let font_size = match style.font_size {
        Some(size) if size.is_finite() && size > 0.0 => size as f32,
        _ => engine.fit_font_size(&converted, &params, fit, &vellum_text::AutoFit::default()),
    };
    let params = params.with_font_size(font_size);
    let layout = engine.layout(&converted, &params);

    // Byte offset where each paragraph begins — the same rule cosmic-text splits on.
    let starts: Vec<usize> = std::iter::once(0)
        .chain(plain.match_indices('\n').map(|(at, _)| at + 1))
        .collect();

    let family = style.font_family.clone().unwrap_or_else(|| "Inter".to_owned());
    let ink = style.text_color.map_or(Color::BLACK, colour);
    let font = FontSpec::new(family, f64::from(font_size));

    let mut lines = Vec::with_capacity(layout.lines.len());
    for line in &layout.lines {
        let start = starts.get(line.paragraph).copied().unwrap_or(0);
        let (from, to) = line.glyphs.iter().fold((usize::MAX, 0), |(lo, hi), glyph| {
            (lo.min(start + glyph.cluster.start), hi.max(start + glyph.cluster.end))
        });
        // A blank line still occupies a baseline, so it is kept with an empty span rather
        // than dropped — otherwise a deliberate empty line between paragraphs closes up.
        let content = if from == usize::MAX || from >= to || to > plain.len() {
            String::new()
        } else {
            plain[from..to].to_owned()
        };
        lines.push(vellum_export::TextLine::new(
            f64::from(line.baseline),
            vec![vellum_export::Span::new(content, font.clone(), ink)],
        ));
    }
    if lines.is_empty() {
        return None;
    }

    let mut block = TextBlock::new(lines);
    block.align = match style.align {
        Some(vellum_doc::Align::Center) => vellum_export::Align::Center,
        Some(vellum_doc::Align::Right) => vellum_export::Align::Right,
        _ => vellum_export::Align::Left,
    };
    Some(block)
}

/// A point stored relative to an item's centre, in world coordinates.
fn rotate_about(placement: &Placement, x: f64, y: f64) -> (f64, f64) {
    let (sin, cos) = placement.rotation.to_radians().sin_cos();
    let (x, y) = (x * placement.scale, y * placement.scale);
    (placement.x + x * cos - y * sin, placement.y + x * sin + y * cos)
}

fn aspect(rect: Rect) -> f32 {
    if rect.height > 0.0 { (rect.width / rect.height) as f32 } else { 1.0 }
}

/// Miro's canonical sticky yellow, matching `crate::inspect`'s.
const MIRO_YELLOW: Color = Color::rgb(0xFF, 0xF7, 0x9E);

/// What a run is set at when the document says nothing.
const DEFAULT_FONT_SIZE: f64 = 14.0;

#[cfg(test)]
mod tests {
    use super::*;
    use vellum_doc::{Board, NewItem, StyledText};
    use vellum_export::BoardSource;

    fn projection_of(items: impl IntoIterator<Item = NewItem>) -> Projection {
        let mut board = Board::new();
        for item in items {
            board.add(item).expect("adding");
        }
        let mut projection = Projection::new();
        projection.rebuild(&board).expect("projecting");
        projection
    }

    fn blobs() -> (tempfile::TempDir, BlobStore) {
        let dir = tempfile::tempdir().expect("a temp dir");
        let store = BlobStore::open(dir.path().join("blobs")).expect("a blob store");
        (dir, store)
    }

    /// The whole point: a board becomes items an exporter can write.
    #[test]
    fn a_board_becomes_export_items() {
        let projection = projection_of([
            NewItem::new(
                ItemKind::Sticky { text: StyledText::plain("Coolant"), background: None },
                Placement::new(0.0, 0.0, 200.0, 200.0),
            ),
            NewItem::new(
                ItemKind::Text { text: StyledText::plain("Notes") },
                Placement::new(400.0, 0.0, 240.0, 48.0),
            ),
        ]);
        let (_dir, store) = blobs();
        let source = BoardExport::new(&projection, &store, Some("garage".into()));

        let items: Vec<Item> = source.items().collect();
        assert_eq!(items.len(), 2);
        assert_eq!(source.title(), Some("garage"));
        assert!(items.iter().any(|i| matches!(i.kind, Kind::Sticky)));
        assert!(items.iter().any(|i| matches!(i.kind, Kind::Text)));
        // Both carry their words, so a CSV or a PDF is not blank.
        assert!(items.iter().all(|i| i.text.is_some()));
    }

    /// Placements are centres and export rects are corners. Getting this wrong puts
    /// every item half its own size away from where it belongs — a whole-board offset
    /// that still *looks* like a board.
    #[test]
    fn a_centre_placement_becomes_a_corner_rect() {
        let rect = rect_of(&Placement::new(100.0, 50.0, 200.0, 80.0));
        assert!((rect.x - 0.0).abs() < 1e-9, "{}", rect.x);
        assert!((rect.y - 10.0).abs() < 1e-9, "{}", rect.y);
        assert!((rect.width - 200.0).abs() < 1e-9);
        assert!((rect.height - 80.0).abs() < 1e-9);

        // Scale is part of the drawn size, so it has to be in the rect too.
        let scaled = rect_of(&Placement { scale: 2.0, ..Placement::new(0.0, 0.0, 100.0, 100.0) });
        assert!((scaled.width - 200.0).abs() < 1e-9, "{}", scaled.width);
        assert!((scaled.x - -100.0).abs() < 1e-9, "{}", scaled.x);
    }

    /// A frame's name titles its PDF page and names its CSV section, so it belongs in
    /// `name`; putting it in `text` would draw it inside the frame instead.
    #[test]
    fn a_frames_title_is_its_name_not_its_text() {
        let projection = projection_of([NewItem::new(
            ItemKind::Frame {
                title: StyledText::plain("Engine bay"),
                order: Some(3),
                speaker_notes: None,
            },
            Placement::new(0.0, 0.0, 1600.0, 900.0),
        )]);
        let (_dir, store) = blobs();
        let source = BoardExport::new(&projection, &store, None);
        let item = source.items().next().expect("one frame");

        assert_eq!(item.name.as_deref(), Some("Engine bay"));
        assert!(item.text.is_none());
        assert!(matches!(item.kind, Kind::Frame { order: 3, clips: true }), "{:?}", item.kind);
    }

    /// Ink is a stroked centreline, not a filled silhouette — exported as a fill it
    /// would come out as a blob bounded by the stroke rather than as a line.
    #[test]
    fn ink_exports_as_a_stroked_path() {
        let projection = projection_of([NewItem::new(
            ItemKind::Ink {
                points: vec![
                    vellum_doc::Point { x: -10.0, y: 0.0 },
                    vellum_doc::Point { x: 10.0, y: 5.0 },
                ],
                color: None,
                thickness: 4.0,
            },
            Placement::new(100.0, 100.0, 20.0, 5.0),
        )]);
        let (_dir, store) = blobs();
        let source = BoardExport::new(&projection, &store, None);
        let item = source.items().next().expect("one stroke");

        assert!(matches!(item.geometry, Geometry::Path(_)));
        assert!(item.style.fill.is_none(), "ink must not be filled");
        let stroke = item.style.stroke.expect("ink is a stroke");
        assert!((stroke.width - 4.0).abs() < 1e-9);
    }

    /// A connector is **routed** here rather than dropped. Its geometry is derived from
    /// live bounds and never stored, so exporting one means doing the same resolution the
    /// painter does — which this used to refuse, asserting zero items.
    #[test]
    fn a_connector_exports_as_its_routed_line() {
        let projection = projection_of([NewItem::new(
            ItemKind::Connector {
                start: vellum_doc::ConnectorEnd::free(vellum_doc::ConnectorEnd::LEFT),
                end: vellum_doc::ConnectorEnd::free(vellum_doc::ConnectorEnd::RIGHT)
                    .with_arrowhead(vellum_doc::ArrowKind::FilledTriangle),
                routing: vellum_doc::Routing::Straight,
                dash: vellum_doc::Dash::Solid,
                thickness: 2.0,
                color: None,
                captions: Vec::new(),
            },
            Placement::new(0.0, 0.0, 100.0, 100.0),
        )]);
        let (_dir, store) = blobs();
        let source = BoardExport::new(&projection, &store, None);
        let item = source.items().next().expect("the connector exports");

        // A line, not a box: the two free ends are the left and right edge midpoints of the
        // connector's own placement, so the path runs across it rather than round it.
        let Geometry::Path(path) = &item.geometry else { panic!("{:?}", item.geometry) };
        let points: Vec<_> = path
            .subpaths
            .iter()
            .flat_map(|sub| {
                std::iter::once(sub.start).chain(sub.segments.iter().map(vellum_export::Segment::end))
            })
            .collect();
        assert!(points.len() >= 2, "{points:?}");
        assert!((points[0].x - -50.0).abs() < 1e-6, "starts at {:?}", points[0]);
        assert!((points[points.len() - 1].x - 50.0).abs() < 1e-6, "ends at {:?}", points.last());
        assert!(points.iter().all(|p| p.y.abs() < 1e-6), "the line is not level: {points:?}");

        // A connector *is* its line: stroked, never filled, and its arrowheads travel.
        assert!(item.style.fill.is_none(), "a connector must not be filled");
        let stroke = item.style.stroke.expect("a connector is a stroke");
        assert!((stroke.width - 2.0).abs() < 1e-9);
        assert_eq!(
            item.kind,
            Kind::Connector {
                start: vellum_export::EndCap::None,
                end: vellum_export::EndCap::Arrow,
            },
        );
    }

    /// The defect this replaced: `TextBlock::plain` splits on `\n` only, so a paragraph
    /// wider than its box exported as **one line running off the page**. Shaping gives it the
    /// breaks it is drawn with.
    #[test]
    fn a_wrapped_paragraph_exports_as_several_lines() {
        let words = "the quick brown fox jumps over the lazy dog and keeps going for a while";
        let projection = projection_of([NewItem::new(
            ItemKind::Sticky {
                text: vellum_doc::StyledText::plain(words),
                background: None,
            },
            Placement::new(0.0, 0.0, 199.0, 228.0),
        )]);
        let (_dir, store) = blobs();

        // Without a font stack: the old behaviour, and still the fallback.
        let unshaped = BoardExport::new(&projection, &store, None);
        let block = unshaped.items().next().unwrap().text.expect("a sticky has words");
        assert_eq!(block.lines.len(), 1, "the unshaped fallback should be one line");

        // With one: the lines it is actually drawn as.
        let mut engine = vellum_text::TextEngine::new().expect("the test machine has fonts");
        let mut shaped = BoardExport::new(&projection, &store, None);
        shaped.shape_text(&mut engine);
        let block = shaped.items().next().unwrap().text.expect("a sticky has words");
        assert!(block.lines.len() > 1, "shaping produced {} line(s)", block.lines.len());

        // Every word survives the round trip, in order — a line reconstructed from the wrong
        // paragraph's cluster offsets would drop or repeat some.
        let rejoined: String = block
            .lines
            .iter()
            .map(vellum_export::TextLine::plain_text)
            .collect::<Vec<_>>()
            .join(" ");
        let sent: Vec<&str> = words.split_whitespace().collect();
        let back: Vec<&str> = rejoined.split_whitespace().collect();
        assert_eq!(back, sent, "the words came back as {rejoined:?}");

        // Baselines increase down the block, which is what SVG's `<text y>` and PDF's `Td`
        // both consume.
        let baselines: Vec<f64> = block.lines.iter().map(|line| line.baseline).collect();
        assert!(baselines.windows(2).all(|w| w[1] > w[0]), "{baselines:?}");
    }

    /// An auto-fitted sticky — Miro's `fs: 0`, which is every sticky on the reference
    /// board — exports at the size it actually resolved to, not at the 14pt default.
    #[test]
    fn an_auto_fitted_block_exports_at_the_size_it_resolved_to() {
        let projection = projection_of([NewItem::new(
            ItemKind::Sticky {
                text: vellum_doc::StyledText::plain("Hi"),
                background: None,
            },
            // A large box for two characters, so auto-fit lands well above the default.
            Placement::new(0.0, 0.0, 600.0, 600.0),
        )]);
        let (_dir, store) = blobs();
        let mut engine = vellum_text::TextEngine::new().expect("the test machine has fonts");
        let mut source = BoardExport::new(&projection, &store, None);
        source.shape_text(&mut engine);

        let block = source.items().next().unwrap().text.expect("a sticky has words");
        let size = block.lines[0].spans[0].font.size;
        assert!(size > DEFAULT_FONT_SIZE * 2.0, "auto-fit resolved to {size}");
    }
}
