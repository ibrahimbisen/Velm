//! Adapting a Velm board onto `vellum-export`'s scene model — **for both front ends**.
//!
//! `vellum-export` writes SVG, PDF, CSV and PNG from a scene of its own: a flat list of
//! [`vellum_export::Item`]s with explicit geometry, style and text. It models that
//! separately on purpose — an exporter that read the CRDT directly would have to be rebuilt
//! every time the document grew a variant, and it could not run off the UI thread.
//!
//! This is the adapter, and it is the only thing in the workspace that has to know both
//! models.
//!
//! # Why it lives here rather than in `vellum-app`
//!
//! It was 722 lines of `vellum-app`, which pulls in winit, a clipboard, a native menu bar
//! and SQLite. `vellum-export` itself compiles to `wasm32` untouched — its whole dependency
//! set is `base64`, `pdf-writer`, `ttf-parser`, `tiny-skia`, `miniz_oxide`, `thiserror` and
//! `vellum-shapes`, none of which touch the platform, and its `std::fs` calls are confined
//! to `write_file` wrappers over generators that return bytes. So the *writers* could always
//! have crossed; only the adapter could not, and only because of one field:
//! `vellum_store::BlobStore`.
//!
//! Moved rather than copied, exactly as [`crate::project`], [`crate::connector`] and
//! [`crate::theme`] were, with `vellum-app` re-exporting it under its old path. Two
//! derivations of "what does this board look like on paper" would drift within a week and
//! the failure would be silent: an export that disagrees with the canvas is believed,
//! because nobody has the board open beside it.
//!
//! # The seam
//!
//! [`ExportAssets`] is the whole design. Two questions the document cannot answer:
//!
//! - **image bytes** for an `ItemKind::Image`'s `asset_id`. The desktop reads its blob
//!   store; a browser hands over whatever it fetched. Neither vocabulary can cross, and the
//!   media type comes back *with* the bytes because only the side that holds them can name
//!   it — a blob store is content-addressed and stores no metadata at all.
//! - **font file bytes** for a face. Only PDF and the CPU rasteriser need them; SVG names a
//!   family and lets the viewer resolve it. [`BundledFonts`] answers Inter out of
//!   [`vellum_text::BUNDLED_FONTS`], which is `include_bytes!`'d on **every** target, so a
//!   browser can embed real glyphs in a PDF with no fetch.
//!
//! [`NoAssets`] is the honest floor, and it is worth stating precisely rather than
//! generously: **a CSV is whole**, and an SVG or a PDF comes out whole in geometry, style
//! and text with a placeholder where each picture would be. That floor is also what makes
//! this testable — every assertion below runs with no filesystem, no blob store and no
//! network.
//!
//! The trait is held **by value** on [`BoardExport`] rather than borrowed. That is what lets
//! `vellum-app` keep its old constructor: a `BlobAssets<'a>(&'a BlobStore)` newtype carries
//! the borrow, a type alias pins the parameter, and [`BoardExport::new`] takes
//! `impl Into<A>` so the call site still passes a bare `&BlobStore`.
//!
//! # Text is shaped, and what that does and does not mean
//!
//! [`BoardExport::shape_text`] runs the same `vellum-text` engine the canvas draws with over
//! every text-bearing item, and turns the result into [`TextBlock`] lines. It used to use
//! `TextBlock::plain`, which splits on `\n` and spaces the baselines — so **a wrapped
//! paragraph exported as one long line running off the side of the page**, and an
//! auto-fitted sticky exported at 14pt whatever size it was drawn at.
//!
//! What shaping is used for is *where the lines break, how tall they are, and what size an
//! auto-fitted block resolved to*. What is **not** emitted is glyph positions: each line
//! goes out as its own source substring, and the SVG or PDF viewer shapes it again with its
//! own copy of the font. That is the right division — an SVG full of per-glyph `x` offsets
//! is unselectable, unsearchable and enormous — but it means the *within-line* metrics are
//! the viewer's, so a line can come out a little wider or narrower there than it is on the
//! board. Line breaks, which are what actually go wrong, are ours.
//!
//! **A caller with no font stack still gets output.** Skip `shape_text` and every block
//! falls back to `TextBlock::plain`'s unshaped split, which is visibly worse and is what
//! keeps the CSV path and `vellum-export`'s own tests fontless.
//!
//! ⚠ The two front ends do **not** shape with the same font policy, and that is deliberate
//! rather than an oversight — see [`document_params`]. `shape_text` uses the desktop's;
//! `shape_text_with` is the door a browser goes through with [`crate::runs::params_for`].

use std::collections::HashMap;

use vellum_doc::{ItemKind, Placement, Style as DocStyle};
use vellum_export::{
    Color, FontSpec, Geometry, ImageData, ImageRef, Item, ItemId, Kind, Path, Point, RasterRequest,
    Rect, Scene, Stroke, Style, SubPath, TextBlock, ZOrder,
};
use vellum_shapes::Shape;
use vellum_text::{FitBox, LayoutParams, TextEngine};

use crate::project::{Projected, Projection};

// ---------------------------------------------------------------------------------------
// The seam
// ---------------------------------------------------------------------------------------

/// Everything an export needs that is not in the document.
///
/// Both methods default to `None`, so an implementor supplies only what it has. That is the
/// contract `vellum_export::BoardSource` already states for the same two questions, kept
/// here rather than widened: a missing image degrades to a placeholder rectangle and a
/// missing font degrades to a standard PDF face or a rasteriser warning. Neither fails an
/// export, because neither is an error — they are facts about what the caller could reach.
///
/// # Why the media type comes back with the bytes
///
/// A blob store is content-addressed and holds no metadata, and a browser's cache holds
/// whatever the `Content-Type` said. Sniffing it here would mean this crate depended on an
/// image decoder — and the desktop already has one (`image::guess_format`) while the browser
/// already has the header. Asking each side for the answer it already has is cheaper than
/// deriving it twice from bytes.
pub trait ExportAssets {
    /// The bytes behind an `ItemKind::Image`'s `asset_id`.
    ///
    /// Called at most once per distinct asset per export when [`BoardExport::load_images`]
    /// has run, and at most once per distinct asset by `Scene::collect` otherwise — so an
    /// implementor may read lazily and need not cache.
    fn image(&self, _asset_id: &str) -> Option<ImageData> {
        None
    }

    /// Font file bytes for one face.
    ///
    /// `size` is not part of the identity — see [`FontSpec::face_key`] — so one answer
    /// serves every size that face is used at.
    fn font(&self, _font: &FontSpec) -> Option<&[u8]> {
        None
    }
}

/// A caller with nothing to supply: no pictures, no embedded fonts.
///
/// Exactly what that yields, because "complete" is the word a comment like this gets wrong:
/// a **CSV is whole**, since it carries no pictures and names no fonts. An **SVG** is whole
/// in geometry, style and text — it names a family and lets the viewer resolve it — with a
/// placeholder rectangle where each picture would be. A **PDF** is the same, with its words
/// set in a standard face rather than an embedded one. A **PNG** draws everything but the
/// pictures and reports the text it could not draw in [`vellum_export::Raster::warnings`].
///
/// Naming that floor is worth more than hiding it: it is the state a browser is in before it
/// has fetched anything, and three of the four outputs are already worth having there.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NoAssets;

impl ExportAssets for NoAssets {}

/// Inter, out of the binary — the one font answer that works on every target.
///
/// [`vellum_text::BUNDLED_FONTS`] is `include_bytes!`'d, so this needs no filesystem and no
/// fetch: a browser tab can embed real glyphs in a PDF, and the CPU rasteriser can draw
/// them, with nothing supplied. It answers **only** for the bundled family, because that is
/// the only file it has; a board naming `Arial` gets `None` and the writers' own fallbacks,
/// which is the truthful answer rather than Inter wearing Arial's name.
///
/// Compose it with a caller's own images by implementing [`ExportAssets`] and delegating the
/// `font` arm here — it is deliberately not the default, because supplying fonts changes
/// what a PDF embeds and the desktop's output must not move without someone asking for it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BundledFonts;

/// CSS weight at or above which the bold face is served.
///
/// 600 rather than 700: `FontSpec` carries CSS numeric weights and semibold is closer to
/// Inter Bold than to Inter Regular. The bundle has exactly two faces, so every request
/// resolves to one of them or to nothing.
const BOLD_WEIGHT: u16 = 600;

impl ExportAssets for BundledFonts {
    fn font(&self, font: &FontSpec) -> Option<&[u8]> {
        if !font.family.eq_ignore_ascii_case(vellum_text::BUNDLED_FAMILY) {
            return None;
        }
        // No italic face is bundled, and there is no synthetic oblique anywhere in this
        // stack — answering with the upright file would silently set an italic run upright
        // while claiming the request was honoured.
        if font.italic {
            return None;
        }
        Some(vellum_text::BUNDLED_FONTS[usize::from(font.weight >= BOLD_WEIGHT)])
    }
}

// ---------------------------------------------------------------------------------------
// The adapter
// ---------------------------------------------------------------------------------------

/// A board, ready for [`vellum_export::Scene::collect`].
///
/// `A` is held by value rather than borrowed so a front end can wrap its own borrow in a
/// newtype and still hand this out — see the module header.
pub struct BoardExport<'a, A: ExportAssets = NoAssets> {
    projection: &'a Projection,
    assets: A,
    title: Option<String>,
    /// Prefetched image bytes. `BoardSource::image` falls through to [`Self::assets`] when a
    /// key is missing here, so [`Self::load_images`] is an optimisation and never a
    /// precondition — a caller that forgets it still gets its pictures.
    images: HashMap<String, ImageData>,
    /// Shaped text: `BoardSource::items` takes `&self` and shaping needs `&mut TextEngine`.
    /// Filled by [`Self::shape_text`]; an item missing from here falls back to the unshaped
    /// split, which is what a caller with no font stack gets.
    text: HashMap<vellum_scene::ItemId, TextBlock>,
}

impl<'a, A: ExportAssets> BoardExport<'a, A> {
    /// ⚠ **`assets` is `impl Into<A>`, and that is load-bearing rather than ornamental.**
    /// It is what lets `vellum-app` expose `BoardExport::new(projection, blobs, title)` with
    /// a bare `&BlobStore` through a type alias plus one `From` impl, so the single call
    /// site in `actions.rs` did not have to move when this file did. A plain `assets: A`
    /// would have forced the newtype into every caller.
    ///
    /// The cost is that `A` is not inferred from the argument, so a caller either names it
    /// (a type alias does) or uses [`Self::plain`].
    pub fn new(projection: &'a Projection, assets: impl Into<A>, title: Option<String>) -> Self {
        Self {
            projection,
            assets: assets.into(),
            title,
            images: HashMap::new(),
            text: HashMap::new(),
        }
    }

    /// Reads every image the board references, once, through the seam.
    ///
    /// Eager because `BoardSource::image` takes `&self` and cannot populate a cache, and
    /// because an export is a discrete act rather than a per-frame cost. Skipping it is
    /// legal — see [`Self::images`] — and costs one call per distinct asset later instead.
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
            if let Some(data) = self.assets.image(&asset) {
                self.images.insert(asset, data);
            }
        }
    }

    /// Shapes every text-bearing item's words with the **desktop's** font policy.
    ///
    /// See [`document_params`] for what that means and why a browser wants the other one.
    pub fn shape_text(&mut self, engine: &mut TextEngine) {
        self.shape_text_with(engine, document_params);
    }

    /// Shapes every text-bearing item's words, so the export gets real line breaks and the
    /// size an auto-fitted block actually resolved to.
    ///
    /// Eager and up front, because `BoardSource::items` takes `&self` and shaping needs the
    /// engine mutably. An export is a discrete act, so paying for every item once here is
    /// cheaper than the per-frame caching the painter needs.
    ///
    /// A caller that skips this still gets output — `TextBlock::plain`'s unshaped split.
    pub fn shape_text_with(
        &mut self,
        engine: &mut TextEngine,
        params: impl Fn(&DocStyle, Option<FitBox>) -> LayoutParams,
    ) {
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
            if let Some(block) = shaped_block(&text, &style, &placement, engine, &params) {
                self.text.insert(scene, block);
            }
        }
    }
}

impl<'a> BoardExport<'a, NoAssets> {
    /// A board with nothing supplied — see [`NoAssets`] for exactly what that yields.
    ///
    /// Exists because [`Self::new`]'s `impl Into<A>` cannot infer `A` from its argument, so
    /// the common "just give me the SVG" case would otherwise need a turbofish.
    pub fn plain(projection: &'a Projection, title: Option<String>) -> Self {
        Self::new(projection, NoAssets, title)
    }
}

impl<A: ExportAssets> vellum_export::BoardSource for BoardExport<'_, A> {
    fn items(&self) -> impl Iterator<Item = Item> + '_ {
        // A connector's targets are looked up through this, which is why `items` needs the
        // whole projection rather than one item at a time.
        let placements = move |id: vellum_doc::ItemId| self.projection.placement_of(id);
        self.projection.iter().filter_map(move |(scene, projected)| {
            convert(*scene, projected, self.text.get(scene), &placements)
        })
    }

    fn image(&self, image: &ImageRef) -> Option<ImageData> {
        // Prefetched if `load_images` ran; asked for now if it did not. Either order is
        // correct, which is what stops a forgotten call being a board of empty rectangles.
        match self.images.get(&image.0) {
            Some(data) => Some(data.clone()),
            None => self.assets.image(&image.0),
        }
    }

    fn font(&self, font: &FontSpec) -> Option<&[u8]> {
        self.assets.font(font)
    }

    fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }
}

// ---------------------------------------------------------------------------------------
// Font policy — the one thing the two front ends genuinely disagree about
// ---------------------------------------------------------------------------------------

/// Shaping parameters for an export running on a machine with a **font database**.
///
/// One field over [`crate::runs::params_for`], and the field is the whole difference: the
/// family the board asked for, falling back to the bundled one. On the desktop the requested
/// family is very nearly always the family that will shape, because the machine's own fonts
/// sit in the database beside the bundle — a board naming `Arial` gets Arial, and Arial's
/// advances decide where its lines break.
///
/// ⚠ **A browser must not use this.** `crate::runs::params_for` forces the bundled family
/// because a tab's font database *is* the bundle: forwarding `Arial` there asks
/// `TextEngine::family_has_bold` about a family that is not present, the weight is dropped,
/// and the shaper falls through to Inter for the glyphs anyway — so the forwarded name
/// changes nothing except that the bold is lost. That reasoning is written out in full at
/// `crate::runs::params_for`; this function is its counterpart, not a copy of it.
///
/// Delegating for the other four fields rather than restating them is deliberate. This is
/// exactly `vellum_app::text::params_for` — verified field for field, including that its
/// `DEFAULT_FONT_FAMILY` *is* [`vellum_text::BUNDLED_FAMILY`] — and a second spelling of
/// `font_size`, `line_height`, `align` and `max_width` is four more places for the two front
/// ends to come apart.
pub fn document_params(style: &DocStyle, fit: Option<FitBox>) -> LayoutParams {
    LayoutParams {
        font_family: Some(
            style.font_family.clone().unwrap_or_else(|| vellum_text::BUNDLED_FAMILY.to_owned()),
        ),
        ..crate::runs::params_for(style, fit)
    }
}

// ---------------------------------------------------------------------------------------
// PNG: the budget is an area, never an edge
// ---------------------------------------------------------------------------------------

/// The largest scale at or below `wanted` whose raster fits `pixel_limit` pixels.
///
/// ⚠ **An area budget, never an edge one, and the difference is not academic.** Capping the
/// *edge* at 8192 admits 8192 × 8192 = 67 megapixels — and with a multisampled attachment on
/// top of the readback texture that is 20 bytes a pixel, which is how a capture on this
/// project once asked for **1.34 GB in one allocation** on an 8 GB machine. It also punishes
/// a wide short board for its shape: a 20000 × 400 region is 8 megapixels and perfectly
/// affordable, and an edge cap refuses it.
///
/// Both axes are scaled together, so an export never silently changes aspect ratio. That is
/// the second half of the same lesson: the tempting fix for "too many pixels" is to clamp the
/// long edge, and a raster whose proportions moved is a wrong picture rather than a small one.
///
/// The fit is decided by **`RasterRequest::pixel_size` itself** rather than by arithmetic of
/// this function's own, so the answer cannot disagree with the check the rasteriser will
/// actually apply — a helper that computed the budget a hair differently would hand back a
/// scale that is then refused, which is worse than no helper.
///
/// Returns `0.0` for a region or a scale that is not a finite positive number. That is a
/// value `RasterRequest::check` refuses with a message carrying the numbers, which is the
/// right destination for nonsense — better than a helper inventing a plausible size.
pub fn raster_scale(region: Rect, wanted: f64, pixel_limit: u64) -> f64 {
    if !wanted.is_finite()
        || wanted <= 0.0
        || !region.width.is_finite()
        || !region.height.is_finite()
    {
        return 0.0;
    }
    let fits = |scale: f64| {
        let probe = RasterRequest { region, scale, background: None, pixel_limit };
        let (w, h) = probe.pixel_size();
        u64::from(w) * u64::from(h) <= pixel_limit
    };
    if fits(wanted) {
        return wanted;
    }
    // Bisection rather than a single √(limit / pixels) shrink: `pixel_size` rounds *up*, so
    // the closed-form answer can land a fractional edge back over the budget and would then
    // need an unbounded correction loop. Sixty halvings take a `f64` to its own resolution.
    let (mut low, mut high) = (0.0_f64, wanted);
    for _ in 0..60 {
        let mid = f64::midpoint(low, high);
        if fits(mid) { low = mid } else { high = mid }
    }
    low
}

/// A whole-scene PNG request at `wanted` scale, shrunk until it fits the default budget.
///
/// The convenience that makes [`raster_scale`] hard to forget: `RasterRequest::scene` takes
/// whatever scale it is handed and fails afterwards, which is right for a writer and wrong
/// for a menu row.
pub fn raster_request(scene: &Scene, wanted: f64) -> RasterRequest {
    let limit = vellum_export::raster::DEFAULT_PIXEL_LIMIT;
    RasterRequest::scene(scene, raster_scale(scene.bounds, wanted, limit))
}

// ---------------------------------------------------------------------------------------
// Item conversion
// ---------------------------------------------------------------------------------------

/// The rectangle an item occupies, before rotation.
fn rect_of(placement: &Placement) -> Rect {
    let (w, h) = placement.scaled_size();
    Rect::new(placement.x - w / 2.0, placement.y - h / 2.0, w, h)
}

fn colour(c: vellum_doc::Color) -> Color {
    Color::rgba(c.r, c.g, c.b, c.a)
}

/// The shape a `form` token names.
///
/// ⚠ **This is the third copy of these four lines in the workspace**, beside
/// `vellum_app::shapes::decode` — whose own header calls itself *"the one place a
/// `vellum_shapes::Shape` becomes a document token and back"*, which stopped being true
/// before this file existed — and the private `decode` in `vellum_web::shapes`. All three
/// are `serde_json::from_str` with a rectangle fallback, because that is the contract
/// `vellum_doc::ItemKind::Shape` states: *"an unreadable one draws as a rectangle rather
/// than costing the item"*. The right home for the single derivation is a `crate::shapes`
/// module here, which both front ends re-export the way they already re-export
/// [`crate::project`]; that is a file move plus two `pub use` lines and it is not this
/// file's to make.
///
/// A token this build does not understand becomes a rectangle rather than an error, so a
/// board written by a later version still exports, still shows something where the shape
/// was, and keeps its size, position, style and label.
fn shape_of(token: &str) -> Shape {
    serde_json::from_str(token).unwrap_or(Shape::Rectangle)
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
            let shape = shape_of(form);
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
        // one means routing it here. That is what `routed_path` does. A route that comes
        // back with fewer than two points is a connector whose ends coincide, and there is
        // nothing to draw.
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
                    let family = style.font_family.clone().unwrap_or_else(default_family);
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
/// **derived from live bounds every time it is asked for** — [`crate::connector`]'s header is
/// explicit that storing it would produce a connector that lies the moment either end moves.
/// So the export has to do the same resolution the painter does.
///
/// Curved routes are flattened, since neither SVG's nor PDF's path syntax is reached through
/// `vellum_export::SubPath`'s line-only model. `FLATTEN_TOLERANCE` is in board units and is
/// well under a printed dot at any sane page size.
///
/// Arrowheads are **not** part of this path: they are `ConnectorStyle` geometry that
/// `vellum-connect` builds as separate filled shapes. They ride on `Kind::Connector` instead,
/// which every writer turns into its own head — see [`end_cap`].
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
    engine: &mut TextEngine,
    params_for: &impl Fn(&DocStyle, Option<FitBox>) -> LayoutParams,
) -> Option<TextBlock> {
    let plain = text.to_plain();
    if plain.trim().is_empty() {
        return None;
    }
    let (width, height) = placement.scaled_size();
    #[expect(clippy::cast_possible_truncation, reason = "an item's box is screen-scale")]
    let fit = FitBox::new(width.max(1.0) as f32, height.max(1.0) as f32);
    let converted = crate::runs::convert(text);
    let params = params_for(style, Some(fit));
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

    let family = style.font_family.clone().unwrap_or_else(default_family);
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

/// The family name written into the output when the item has not named one.
///
/// Named from [`vellum_text::BUNDLED_FAMILY`] rather than spelt `"Inter"` here, because this
/// string ends up in an SVG's `font-family` and in the PDF's face lookup — so a divergence
/// between it and the family that was actually *shaped* is a document whose lines break in
/// one place and are set in another.
fn default_family() -> String {
    vellum_text::BUNDLED_FAMILY.to_owned()
}

/// Miro's canonical sticky yellow, matching `vellum_app::inspect`'s.
const MIRO_YELLOW: Color = Color::rgb(0xFF, 0xF7, 0x9E);

/// What a run is set at when the document says nothing.
const DEFAULT_FONT_SIZE: f64 = 14.0;

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use vellum_doc::{Board, NewItem, StyledText};
    use vellum_export::{BoardSource, FaceKey};

    fn projection_of(items: impl IntoIterator<Item = NewItem>) -> Projection {
        let mut board = Board::new();
        for item in items {
            board.add(item).expect("adding");
        }
        let mut projection = Projection::new();
        projection.rebuild(&board).expect("projecting");
        projection
    }

    /// The seam, in memory.
    ///
    /// **No blob store and no temp directory**, unlike the version of these tests that lived
    /// in `vellum-app`: the whole point of the trait is that the adapter never learns where
    /// bytes come from, and a test that opened a `BlobStore` to prove it would be asserting
    /// the opposite. `asked` counts calls, which is the only way to tell a prefetch that
    /// cached from one that merely worked.
    #[derive(Default)]
    struct TestAssets {
        images: HashMap<String, ImageData>,
        fonts: HashMap<FaceKey, Vec<u8>>,
        asked: Cell<usize>,
    }

    impl TestAssets {
        fn with_image(mut self, key: &str, bytes: &[u8]) -> Self {
            self.images.insert(key.to_owned(), ImageData::encoded("image/png", bytes.to_vec()));
            self
        }

        fn with_font(mut self, font: &FontSpec, bytes: &[u8]) -> Self {
            self.fonts.insert(font.face_key(), bytes.to_vec());
            self
        }
    }

    impl ExportAssets for TestAssets {
        fn image(&self, asset_id: &str) -> Option<ImageData> {
            self.asked.set(self.asked.get() + 1);
            self.images.get(asset_id).cloned()
        }

        fn font(&self, font: &FontSpec) -> Option<&[u8]> {
            self.fonts.get(&font.face_key()).map(Vec::as_slice)
        }
    }

    fn sticky(text: &str, placement: Placement) -> NewItem {
        NewItem::new(ItemKind::Sticky { text: StyledText::plain(text), background: None }, placement)
    }

    // -----------------------------------------------------------------------------------
    // The conversion
    // -----------------------------------------------------------------------------------

    /// The whole point: a board becomes items an exporter can write.
    #[test]
    fn a_board_becomes_export_items() {
        let projection = projection_of([
            sticky("Coolant", Placement::new(0.0, 0.0, 200.0, 200.0)),
            NewItem::new(
                ItemKind::Text { text: StyledText::plain("Notes") },
                Placement::new(400.0, 0.0, 240.0, 48.0),
            ),
        ]);
        let source = BoardExport::plain(&projection, Some("garage".into()));

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
        let source = BoardExport::plain(&projection, None);
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
        let source = BoardExport::plain(&projection, None);
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
        let source = BoardExport::plain(&projection, None);
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

    /// A shape exports as its catalogue silhouette, and the token is what names it.
    ///
    /// The failure this guards is quiet: an unreadable token degrades to a rectangle by
    /// contract, so a `shape_of` that had stopped parsing altogether would still produce a
    /// perfectly plausible board of rectangles. Asserting the *name* is what tells the two
    /// apart, and the second half asserts the degrade is still the degrade.
    ///
    /// ⚠ **Sorted, never indexed.** `Projection::iter` walks a `HashMap`, so item order is
    /// not the order they were added in and is not stable between runs — an assertion on
    /// `items[0]` here would pass most of the time.
    #[test]
    fn a_shape_exports_as_its_catalogue_outline_and_an_unreadable_one_degrades() {
        let star = Shape::Star { points: 7, inner_ratio: 0.31 };
        let token = serde_json::to_string(&star).expect("a plain data enum encodes");
        let projection = projection_of([
            NewItem::new(
                ItemKind::Shape { form: token, text: StyledText::plain("") },
                Placement::new(0.0, 0.0, 120.0, 90.0),
            ),
            NewItem::new(
                ItemKind::Shape {
                    form: "a shape from the future".to_owned(),
                    text: StyledText::plain(""),
                },
                Placement::new(300.0, 0.0, 120.0, 90.0),
            ),
        ]);
        let source = BoardExport::plain(&projection, None);
        let items: Vec<Item> = source.items().collect();
        let mut names: Vec<&str> = items
            .iter()
            .map(|item| match &item.kind {
                Kind::Shape { name } => *name,
                other => panic!("{other:?}"),
            })
            .collect();
        names.sort_unstable();
        assert_eq!(names, vec![Shape::Rectangle.name(), star.name()]);

        // Real geometry, not the fallback box: a star's outline has more corners than a
        // rectangle's four, which is what says the token was parsed rather than defaulted.
        let most = items
            .iter()
            .map(|item| match &item.geometry {
                Geometry::Path(path) => {
                    path.subpaths.iter().map(|s| s.segments.len()).sum::<usize>()
                }
                other => panic!("{other:?}"),
            })
            .max()
            .expect("two shapes");
        assert!(most > 4, "the busiest shape came out with {most} segment(s)");
    }

    // -----------------------------------------------------------------------------------
    // The seam
    // -----------------------------------------------------------------------------------

    /// An image resolves through the seam **whether or not the prefetch ran**, and the
    /// prefetch is what stops it being asked for twice.
    ///
    /// Both halves matter and neither implies the other. A build that only answered from the
    /// cache would give a caller who forgot `load_images` a board of empty rectangles with no
    /// error anywhere; a build that only asked through would re-read every asset once per
    /// distinct key, which on the desktop is file I/O.
    #[test]
    fn an_image_resolves_with_or_without_the_prefetch_and_the_prefetch_caches() {
        let projection = projection_of([NewItem::new(
            ItemKind::Image { asset_id: "abc123".to_owned(), crop: None },
            Placement::new(0.0, 0.0, 100.0, 100.0),
        )]);
        let reference = ImageRef::new("abc123");

        // Never prefetched: the seam is still asked, and it still answers.
        let cold: BoardExport<'_, TestAssets> =
            BoardExport::new(&projection, TestAssets::default().with_image("abc123", &[1, 2, 3]), None);
        assert!(cold.image(&reference).is_some(), "a forgotten prefetch must not lose the picture");
        assert!(cold.image(&reference).is_some());
        assert_eq!(cold.assets.asked.get(), 2, "without a prefetch each ask reaches the seam");

        // Prefetched: asked once, answered from memory thereafter.
        let mut warm: BoardExport<'_, TestAssets> =
            BoardExport::new(&projection, TestAssets::default().with_image("abc123", &[1, 2, 3]), None);
        warm.load_images();
        assert_eq!(warm.assets.asked.get(), 1);
        assert!(warm.image(&reference).is_some());
        assert!(warm.image(&reference).is_some());
        assert_eq!(warm.assets.asked.get(), 1, "the prefetch is meant to be the only read");

        // An asset the caller cannot supply is a missing picture, never a failed export.
        let empty: BoardExport<'_, TestAssets> =
            BoardExport::new(&projection, TestAssets::default(), None);
        assert!(empty.image(&reference).is_none());
        assert_eq!(empty.items().count(), 1, "the item still exports without its bytes");
    }

    /// Font bytes reach the writers through the same seam the images do.
    ///
    /// This is the hop that had **no implementor at all** before the seam existed:
    /// `BoardSource::font` defaults to `None`, so every PDF fell back to a standard face and
    /// nothing in the workspace could have told you.
    #[test]
    fn a_font_reaches_the_writers_through_the_seam() {
        let projection = projection_of([sticky("Torque", Placement::new(0.0, 0.0, 100.0, 100.0))]);
        let spec = FontSpec::new(vellum_text::BUNDLED_FAMILY, 14.0);
        let source: BoardExport<'_, TestAssets> =
            BoardExport::new(&projection, TestAssets::default().with_font(&spec, &[0xDE, 0xAD]), None);

        // Any size of the same face, because a face is keyed without one.
        let other_size = FontSpec::new(vellum_text::BUNDLED_FAMILY, 96.0);
        assert_eq!(source.font(&other_size), Some([0xDE, 0xAD].as_slice()));
        assert_eq!(source.font(&other_size.clone().bold()), None, "bold is another face");

        // And nothing supplied is a supported answer, not a failure.
        let bare = BoardExport::plain(&projection, None);
        assert_eq!(bare.font(&spec), None);
    }

    /// The bundled faces answer by weight, for the bundled family, and for nothing else.
    ///
    /// A family check that let anything through would serve Inter under Arial's name, which
    /// is a document that is set in a typeface nobody chose while reporting success — worse
    /// than the standard-font fallback it replaced.
    #[test]
    fn the_bundled_faces_answer_by_weight_and_only_for_their_own_family() {
        let regular = BundledFonts.font(&FontSpec::new("Inter", 14.0)).expect("regular is bundled");
        let bold = BundledFonts.font(&FontSpec::new("Inter", 14.0).bold()).expect("bold is bundled");
        assert_eq!(regular, vellum_text::BUNDLED_FONTS[0]);
        assert_eq!(bold, vellum_text::BUNDLED_FONTS[1]);
        assert_ne!(regular, bold, "two faces, not one file twice");

        // Semibold is nearer the bold file than the regular one.
        let semibold = FontSpec { weight: 600, ..FontSpec::new("Inter", 14.0) };
        assert_eq!(BundledFonts.font(&semibold), Some(vellum_text::BUNDLED_FONTS[1]));

        // Case-insensitive, because a board carries whatever Miro's `ffn` said.
        assert!(BundledFonts.font(&FontSpec::new("inter", 14.0)).is_some());

        // And refused for everything else: another family, and an italic there is no file for.
        assert_eq!(BundledFonts.font(&FontSpec::new("Arial", 14.0)), None);
        assert_eq!(BundledFonts.font(&FontSpec::new("Inter", 14.0).italic()), None);
    }

    // -----------------------------------------------------------------------------------
    // Text
    // -----------------------------------------------------------------------------------

    /// The defect this replaced: `TextBlock::plain` splits on `\n` only, so a paragraph
    /// wider than its box exported as **one line running off the page**. Shaping gives it the
    /// breaks it is drawn with.
    #[test]
    fn a_wrapped_paragraph_exports_as_several_lines() {
        let words = "the quick brown fox jumps over the lazy dog and keeps going for a while";
        let projection =
            projection_of([sticky(words, Placement::new(0.0, 0.0, 199.0, 228.0))]);

        // Without a font stack: the old behaviour, and still the fallback.
        let unshaped = BoardExport::plain(&projection, None);
        let block = unshaped.items().next().unwrap().text.expect("a sticky has words");
        assert_eq!(block.lines.len(), 1, "the unshaped fallback should be one line");

        // With one: the lines it is actually drawn as.
        let mut engine = TextEngine::new().expect("the test machine has fonts");
        let mut shaped = BoardExport::plain(&projection, None);
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
        // A large box for two characters, so auto-fit lands well above the default.
        let projection = projection_of([sticky("Hi", Placement::new(0.0, 0.0, 600.0, 600.0))]);
        let mut engine = TextEngine::new().expect("the test machine has fonts");
        let mut source = BoardExport::plain(&projection, None);
        source.shape_text(&mut engine);

        let block = source.items().next().unwrap().text.expect("a sticky has words");
        let size = block.lines[0].spans[0].font.size;
        assert!(size > DEFAULT_FONT_SIZE * 2.0, "auto-fit resolved to {size}");
    }

    /// ⚠ **The two front ends shape with different font policies, and this is the assertion
    /// that keeps them from being quietly unified.**
    ///
    /// `document_params` forwards the family the board asked for; `crate::runs::params_for`
    /// substitutes the bundled one, because a browser's font database *is* the bundle and
    /// forwarding a name it has not got drops the weight for nothing. Everything else about
    /// them must agree — a second spelling of `font_size`, `line_height`, `align` and
    /// `max_width` is four more places for the two to come apart — so the test asserts the
    /// difference is exactly one field.
    #[test]
    fn the_desktop_policy_keeps_the_family_the_board_asked_for_and_differs_in_nothing_else() {
        let style = DocStyle {
            font_family: Some("Arial".to_owned()),
            font_size: Some(21.0),
            line_height: Some(1.5),
            align: Some(vellum_doc::Align::Center),
            ..DocStyle::default()
        };
        let fit = FitBox::new(200.0, 100.0);

        let desktop = document_params(&style, Some(fit));
        let browser = crate::runs::params_for(&style, Some(fit));

        assert_eq!(desktop.font_family.as_deref(), Some("Arial"));
        assert_eq!(browser.font_family.as_deref(), Some(vellum_text::BUNDLED_FAMILY));
        assert_eq!(
            LayoutParams { font_family: browser.font_family.clone(), ..desktop.clone() },
            browser,
            "the two policies must differ in the family and in nothing else",
        );

        // An item that names no family falls back to the bundled one, so the two agree —
        // which is every item on a board Velm made rather than imported.
        let unnamed = DocStyle { font_family: None, ..style };
        assert_eq!(
            document_params(&unnamed, Some(fit)).font_family.as_deref(),
            Some(vellum_text::BUNDLED_FAMILY),
        );
    }

    // -----------------------------------------------------------------------------------
    // The raster budget
    // -----------------------------------------------------------------------------------

    /// The budget is an **area**, and the aspect ratio survives it.
    ///
    /// The A/B is inline and it is the whole test: an 8192-pixel *edge* cap — the shape of
    /// cap that asked for 1.34 GB in one allocation on this project once — admits the first
    /// case below at 67 megapixels, and refuses the second at 8. Both are wrong in opposite
    /// directions, and only an area budget gets both right.
    #[test]
    fn a_raster_scale_is_capped_by_area_and_keeps_its_aspect() {
        const LIMIT: u64 = 20_000_000;

        let fits = |region: Rect, scale: f64| {
            let (w, h) = RasterRequest { region, scale, background: None, pixel_limit: LIMIT }
                .pixel_size();
            (u64::from(w) * u64::from(h), w, h)
        };

        // A big square board asked for at 4x: far over the budget, so it is shrunk.
        let square = Rect::new(0.0, 0.0, 16_000.0, 16_000.0);
        let scale = raster_scale(square, 4.0, LIMIT);
        assert!(scale > 0.0 && scale < 4.0, "{scale}");
        let (pixels, w, h) = fits(square, scale);
        assert!(pixels <= LIMIT, "{w}x{h} is {pixels} pixels");
        // Aspect held: a square board stays square, to within the rounding-up of one edge.
        assert!(w.abs_diff(h) <= 1, "{w}x{h} is no longer square");
        // And it is the *largest* such scale: a hair more must not fit.
        assert!(fits(square, scale * 1.01).0 > LIMIT, "the shrink went further than it had to");

        // ⚠ A wide short board is not punished for its shape. This is 8 megapixels at 1x —
        // comfortably inside the budget — and an edge cap of 8192 would refuse it outright.
        let wide = Rect::new(0.0, 0.0, 20_000.0, 400.0);
        assert!((raster_scale(wide, 1.0, LIMIT) - 1.0).abs() < 1e-12, "a wide board was shrunk");
        assert!(fits(wide, 1.0).1 > 8192, "the fixture no longer exercises the edge case");

        // Something already inside the budget is handed straight back.
        let small = Rect::new(0.0, 0.0, 800.0, 600.0);
        assert!((raster_scale(small, 2.0, LIMIT) - 2.0).abs() < 1e-12);

        // Nonsense is refused rather than guessed at, and `RasterRequest::check` is what
        // reports it with the numbers.
        assert_eq!(raster_scale(small, f64::NAN, LIMIT), 0.0);
        assert_eq!(raster_scale(small, -1.0, LIMIT), 0.0);
        assert_eq!(raster_scale(Rect::new(0.0, 0.0, f64::INFINITY, 10.0), 1.0, LIMIT), 0.0);
    }
}
