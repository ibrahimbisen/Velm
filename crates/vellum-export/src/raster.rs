//! PNG export: an interface the GPU will implement, and a CPU fallback that works
//! today.
//!
//! # Why the interface comes first
//!
//! `docs/01-architecture.md` §3 puts rasterisation on `wgpu` — instanced quads, SDF
//! shapes, a glyph atlas. That renderer will eventually export PNG far faster than
//! anything here, and at higher fidelity, because it is the same code that draws the
//! canvas and therefore cannot disagree with it.
//!
//! It is not what should be *depended on*, though. An export must work when there
//! is no GPU: in CI, in a headless thumbnail job, on a machine whose driver has
//! just been denylisted. So [`Rasteriser`] is the contract, [`CpuRasteriser`] is the
//! implementation that is always available, and `vellum-render` supplies a faster
//! one by implementing the same trait. The scene, the clipping and the z-order are
//! already resolved by [`Scene`], so the two implementations share everything except
//! the pixel-pushing.
//!
//! # What the CPU path can and cannot do
//!
//! `tiny-skia` is a port of Skia's raster pipeline: anti-aliased fills, strokes,
//! dashes, clips and image blits, all correct. It has no text engine at all, so
//! glyphs are drawn as filled outlines read from the font file the caller supplied
//! ([`crate::font`]). Where no font was supplied, the text is skipped and named in
//! [`Raster::warnings`] — a silently blank sticky would be worse than a warning.
//!
//! Known gaps, listed rather than papered over:
//!
//! - **No shaping.** Glyph advances are unkerned; see [`crate::font`].
//! - **Encoded images are decoded only if they are PNG.** `tiny-skia` decodes PNG
//!   and nothing else. A caller that wants JPEG rasterised passes
//!   [`ImageData::Rgba8`], which it already has if the image was decoded for a GPU
//!   texture.
//! - **No blur.** Nothing in the item model has a shadow yet, so there is nothing
//!   to blur; when shadows land (`docs/features/README.md` §4, P4) this is where
//!   they arrive.

use crate::error::ExportError;
use crate::geom::{Affine, Path, Rect, Segment};
use crate::item::Kind;
use crate::scene::{Placed, Scene};
use crate::source::ImageData;
use crate::style::{Color, Dash, LineCap, LineJoin, Stroke};
use crate::text::{Align, TextBlock};
use tiny_skia::{
    FillRule, Mask, Paint, PathBuilder, Pixmap, PixmapPaint, Shader, Stroke as SkStroke,
    StrokeDash, Transform,
};

/// A pixel budget, chosen so a mistaken scale cannot exhaust an 8GB machine.
///
/// 268,435,456 pixels is 1GB of RGBA8 — about 16k × 16k. The reference board is
/// 41,282 × 17,515 board units, so exporting the whole of it at 1× is already over
/// this and must be refused with a number rather than by swapping. The limit is a
/// field on [`RasterRequest`] so a caller with the memory can raise it.
pub const DEFAULT_PIXEL_LIMIT: u64 = 256 * 1024 * 1024;

/// What to rasterise.
#[derive(Debug, Clone)]
pub struct RasterRequest {
    /// The board-space region to cover. Use [`Scene::bounds`] for everything, or a
    /// frame's rect for one frame.
    pub region: Rect,
    /// Output pixels per board unit. Miro's image export offers ×1, ×2 and ×4; any
    /// positive value works here.
    pub scale: f64,
    /// Painted first. `None` leaves the PNG transparent.
    pub background: Option<Color>,
    pub pixel_limit: u64,
}

impl RasterRequest {
    /// The whole scene at `scale`.
    pub fn scene(scene: &Scene, scale: f64) -> Self {
        Self {
            region: scene.bounds,
            scale,
            background: None,
            pixel_limit: DEFAULT_PIXEL_LIMIT,
        }
    }

    pub fn with_background(mut self, color: Color) -> Self {
        self.background = Some(color);
        self
    }

    /// The output size in whole pixels, rounded up so a fractional edge is included
    /// rather than cropped, and never zero.
    pub fn pixel_size(&self) -> (u32, u32) {
        let dimension = |v: f64| {
            let px = (v * self.scale).ceil();
            if px.is_finite() && px >= 1.0 { px.min(f64::from(u32::MAX)) as u32 } else { 1 }
        };
        (dimension(self.region.width), dimension(self.region.height))
    }

    fn check(&self) -> Result<(u32, u32), ExportError> {
        if !(self.scale.is_finite() && self.scale > 0.0) {
            return Err(ExportError::RasterTooLarge {
                width: 0,
                height: 0,
                pixels: 0,
                limit: self.pixel_limit,
            });
        }
        let (width, height) = self.pixel_size();
        let pixels = u64::from(width) * u64::from(height);
        if pixels > self.pixel_limit {
            return Err(ExportError::RasterTooLarge {
                width,
                height,
                pixels,
                limit: self.pixel_limit,
            });
        }
        Ok((width, height))
    }
}

/// Straight RGBA8 pixels, row-major.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Raster {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    /// Everything the rasteriser could not draw faithfully, each said once. Empty
    /// means the image is complete.
    pub warnings: Vec<String>,
}

impl Raster {
    pub fn encode_png(&self) -> Result<Vec<u8>, ExportError> {
        encode_rgba_png(self.width, self.height, &self.rgba)
    }

    /// The pixel at `(x, y)`, for tests and for eyedropper-style checks.
    pub fn pixel(&self, x: u32, y: u32) -> Option<Color> {
        let index = (y as usize)
            .checked_mul(self.width as usize)?
            .checked_add(x as usize)?
            .checked_mul(4)?;
        let p = self.rgba.get(index..index + 4)?;
        Some(Color::rgba(p[0], p[1], p[2], p[3]))
    }
}

/// Turns a resolved scene into pixels.
///
/// Implemented here by [`CpuRasteriser`] and, later, by the `wgpu` renderer. The
/// trait takes a `&Scene` rather than a board so that both implementations are
/// looking at the identical z-order and clipping.
pub trait Rasteriser {
    fn rasterise(&mut self, scene: &Scene, request: &RasterRequest) -> Result<Raster, ExportError>;
}

/// Rasterises PNG on the CPU, via `tiny-skia`.
#[derive(Debug, Clone, Copy, Default)]
pub struct CpuRasteriser;

impl Rasteriser for CpuRasteriser {
    fn rasterise(&mut self, scene: &Scene, request: &RasterRequest) -> Result<Raster, ExportError> {
        let (width, height) = request.check()?;
        let mut pixmap = Pixmap::new(width, height)
            .ok_or(ExportError::RasterAllocation { width, height })?;
        if let Some(background) = request.background {
            pixmap.fill(to_skia_color(background));
        }

        // Board space to pixel space: shift the region's origin to zero, then scale.
        let view = Affine::translation(-request.region.x, -request.region.y)
            .then(Affine::scaling(request.scale, request.scale));

        let mut painter = Painter::new(&mut pixmap);
        for placed in &scene.items {
            painter.draw_item(scene, placed, &view);
        }
        let warnings = painter.finish();

        Ok(Raster { width, height, rgba: demultiply(&pixmap), warnings })
    }
}

/// Rasterises and encodes in one step — the common case.
pub fn write(
    rasteriser: &mut impl Rasteriser,
    scene: &Scene,
    request: &RasterRequest,
) -> Result<Vec<u8>, ExportError> {
    rasteriser.rasterise(scene, request)?.encode_png()
}

/// Writes a PNG to a file.
pub fn write_file(
    rasteriser: &mut impl Rasteriser,
    scene: &Scene,
    request: &RasterRequest,
    path: impl AsRef<std::path::Path>,
) -> Result<(), ExportError> {
    let path = path.as_ref();
    let png = write(rasteriser, scene, request)?;
    std::fs::write(path, png).map_err(|e| ExportError::io(path.display(), e))
}

/// Encodes straight RGBA8 as PNG.
pub fn encode_rgba_png(width: u32, height: u32, rgba: &[u8]) -> Result<Vec<u8>, ExportError> {
    let expected = (width as usize) * (height as usize) * 4;
    if rgba.len() != expected {
        return Err(ExportError::Png(format!(
            "{width}×{height} needs {expected} bytes of RGBA, got {}",
            rgba.len()
        )));
    }
    let mut pixmap =
        Pixmap::new(width, height).ok_or(ExportError::RasterAllocation { width, height })?;
    for (out, chunk) in pixmap.pixels_mut().iter_mut().zip(rgba.chunks_exact(4)) {
        *out = tiny_skia::ColorU8::from_rgba(chunk[0], chunk[1], chunk[2], chunk[3])
            .premultiply();
    }
    pixmap.encode_png().map_err(|e| ExportError::Png(e.to_string()))
}

// ---------------------------------------------------------------------------
// Drawing
// ---------------------------------------------------------------------------

/// Collects each distinct complaint once. A board with 400 stickies in an
/// unavailable font should say so once, not four hundred times.
#[derive(Debug, Default)]
struct Warnings(std::collections::BTreeSet<String>);

impl Warnings {
    fn add(&mut self, message: impl Into<String>) {
        self.0.insert(message.into());
    }
}

/// Draws a scene into one pixmap.
///
/// It exists to own the **clip-mask cache**. Every clip in Vellum is a frame's
/// rectangle, and a frame holds many items, so the alpha mask for that rectangle is
/// built once and reused by all of them; `tiny-skia` masks are full-resolution, and
/// rebuilding one per item on a board with twelve frames and 596 objects would
/// dominate the export.
struct Painter<'p> {
    pixmap: &'p mut Pixmap,
    /// Keyed by the clip's pixel-space bits, so identical rectangles share a mask.
    masks: Vec<([u64; 4], Option<Mask>)>,
    warnings: Warnings,
}

impl<'p> Painter<'p> {
    fn new(pixmap: &'p mut Pixmap) -> Self {
        Self { pixmap, masks: Vec::new(), warnings: Warnings::default() }
    }

    fn finish(self) -> Vec<String> {
        self.warnings.0.into_iter().collect()
    }

    /// The mask index for a clip rectangle, building it on first use.
    ///
    /// `None` means "draw unclipped". A clip that cannot be turned into a mask —
    /// because it is degenerate, or because the allocation failed — is deliberately
    /// resolved that way rather than to an empty mask: losing the clip shows a
    /// little too much, while an empty mask would silently drop every item inside
    /// the frame.
    fn mask_for(&mut self, clip: Rect) -> Option<usize> {
        let key = [clip.x.to_bits(), clip.y.to_bits(), clip.width.to_bits(), clip.height.to_bits()];
        if let Some(index) = self.masks.iter().position(|(k, _)| *k == key) {
            return self.masks[index].1.is_some().then_some(index);
        }
        let built = self.build_mask(clip);
        let present = built.is_some();
        self.masks.push((key, built));
        present.then_some(self.masks.len() - 1)
    }

    fn build_mask(&self, clip: Rect) -> Option<Mask> {
        let rect = tiny_skia::Rect::from_xywh(
            clip.x as f32,
            clip.y as f32,
            clip.width as f32,
            clip.height as f32,
        )?;
        let mut builder = PathBuilder::new();
        builder.push_rect(rect);
        let path = builder.finish()?;
        let mut mask = Mask::new(self.pixmap.width(), self.pixmap.height())?;
        mask.fill_path(&path, FillRule::Winding, true, Transform::identity());
        Some(mask)
    }

    fn draw_item(&mut self, scene: &Scene, placed: &Placed, view: &Affine) {
        let item = &placed.item;
        if item.style.opacity <= 0.0 {
            return;
        }
        // A clip that has been resolved away — an item entirely inside its frame —
        // costs nothing to skip, and skips building the mask at all.
        let clip = placed.clip.and_then(|c| {
            let pixels = transform_rect(c, view);
            (!pixels.contains_rect(&transform_rect(item.painted_bounds(), view)))
                .then_some(pixels)
        });
        let mask = clip.and_then(|c| self.mask_for(c));

        let transform = item.transform().then(*view);
        let geometry = item.silhouette().to_path().transformed(&transform);

        match &item.kind {
            Kind::Ink | Kind::Connector { .. } => {
                self.stroke_path(&geometry, &item.style, mask);
                if let Kind::Connector { start, end } = item.kind {
                    self.draw_end_caps(&geometry, start, end, &item.style, mask);
                }
            }
            _ => {
                self.fill_path(&geometry, &item.style, mask);
                self.stroke_path(&geometry, &item.style, mask);
            }
        }

        if item.image.is_some() {
            self.draw_image(scene, placed, view, mask);
        }
        if let Some(block) = &item.text {
            self.draw_text(scene, placed, block, &transform, mask);
        }
    }

    fn fill_path(&mut self, path: &Path, style: &crate::style::Style, mask: Option<usize>) {
        let Some(color) = style.effective_fill() else { return };
        let Some(sk) = to_skia_path(path, true) else { return };
        let paint = solid(color, style.opacity);
        let mask = mask.and_then(|i| self.masks[i].1.as_ref());
        self.pixmap.fill_path(&sk, &paint, FillRule::Winding, Transform::identity(), mask);
    }

    fn stroke_path(&mut self, path: &Path, style: &crate::style::Style, mask: Option<usize>) {
        let Some(stroke) = style.effective_stroke() else { return };
        let Some(sk) = to_skia_path(path, false) else { return };
        let paint = solid(stroke.color, style.opacity);
        let sk_stroke = to_skia_stroke(stroke);
        let mask = mask.and_then(|i| self.masks[i].1.as_ref());
        self.pixmap.stroke_path(&sk, &paint, &sk_stroke, Transform::identity(), mask);
    }

    fn draw_end_caps(
        &mut self,
        path: &Path,
        start: crate::item::EndCap,
        end: crate::item::EndCap,
        style: &crate::style::Style,
        mask: Option<usize>,
    ) {
        use crate::item::EndCap;
        let Some((head, tail)) = path.terminals() else { return };
        let Some(stroke) = style.effective_stroke() else { return };
        // The path is already in pixel space, so the head scales with it too.
        let size = stroke.width * 5.0;
        let filled = crate::style::Style::filled(stroke.color).with_opacity(style.opacity);

        for (terminal, cap) in [(head, start), (tail, end)] {
            if cap == EndCap::None {
                continue;
            }
            self.fill_path(&cap.path(terminal, size), &filled, mask);
        }
    }

    fn draw_image(
        &mut self,
        scene: &Scene,
        placed: &Placed,
        view: &Affine,
        mask: Option<usize>,
    ) {
        let Some(reference) = &placed.item.image else { return };
        let Some(data) = scene.image(reference) else {
            self.warnings.add(format!("image {} could not be resolved", reference.0));
            return;
        };
        let source = match data {
            ImageData::Rgba8 { width, height, pixels } => {
                let Some(mut source) = Pixmap::new(*width, *height) else { return };
                for (out, chunk) in source.pixels_mut().iter_mut().zip(pixels.chunks_exact(4)) {
                    *out = tiny_skia::ColorU8::from_rgba(chunk[0], chunk[1], chunk[2], chunk[3])
                        .premultiply();
                }
                source
            }
            ImageData::Encoded { media_type, bytes } => match Pixmap::decode_png(bytes) {
                Ok(p) => p,
                Err(_) => {
                    self.warnings.add(format!(
                        "{media_type} cannot be decoded on the CPU path — pass ImageData::Rgba8 \
                         to include it in a PNG export"
                    ));
                    return;
                }
            },
        };
        if source.width() == 0 || source.height() == 0 {
            return;
        }

        // The image fills the item's box; a crop is already baked into the stored
        // bytes, per `docs/features/README.md` §1.
        let target = transform_rect(placed.item.geometry.bounds(), view);
        let transform = Transform::from_row(
            (target.width / f64::from(source.width())) as f32,
            0.0,
            0.0,
            (target.height / f64::from(source.height())) as f32,
            target.x as f32,
            target.y as f32,
        );
        let paint = PixmapPaint {
            opacity: placed.item.style.opacity.clamp(0.0, 1.0),
            quality: tiny_skia::FilterQuality::Bilinear,
            ..PixmapPaint::default()
        };
        let mask = mask.and_then(|i| self.masks[i].1.as_ref());
        self.pixmap.draw_pixmap(0, 0, source.as_ref(), &paint, transform, mask);
    }

    fn draw_text(
        &mut self,
        scene: &Scene,
        placed: &Placed,
        block: &TextBlock,
        transform: &Affine,
        mask: Option<usize>,
    ) {
        if block.is_empty() {
            return;
        }
        let box_ = placed.item.text_box();
        let offset = block.valign_offset(box_.height);

        for line in &block.lines {
            if line.is_blank() {
                continue;
            }
            // Advances first, so alignment resolves before anything is drawn.
            let mut widths = Vec::with_capacity(line.spans.len());
            for span in &line.spans {
                let bytes = scene.font(&span.font);
                if bytes.is_none() {
                    self.warnings.add(format!(
                        "no font supplied for {:?}; its text is missing from the raster",
                        span.font.family
                    ));
                }
                widths.push(crate::font::measure(bytes, &span.text, &span.font));
            }
            let total: f64 = widths.iter().sum();
            let mut x = match block.align {
                Align::Left => box_.x,
                Align::Center => box_.centre().x - total / 2.0,
                Align::Right => box_.right() - total,
            };
            let baseline = box_.y + offset + line.baseline;

            for (span, width) in line.spans.iter().zip(&widths) {
                if let Some(bytes) = scene.font(&span.font) {
                    let outlines = glyph_run_paths(bytes, span, x, baseline, transform);
                    let paint = solid(span.color, placed.item.style.opacity);
                    let mask = mask.and_then(|i| self.masks[i].1.as_ref());
                    for outline in &outlines {
                        if let Some(sk) = to_skia_path(outline, true) {
                            self.pixmap.fill_path(
                                &sk,
                                &paint,
                                FillRule::Winding,
                                Transform::identity(),
                                mask,
                            );
                        }
                    }
                }
                x += width;
            }
        }
    }
}

/// One span's glyphs as paths, already placed. Built up front so the borrow of the
/// font outlives no draw call.
fn glyph_run_paths(
    font_bytes: &[u8],
    span: &crate::text::Span,
    x: f64,
    baseline: f64,
    transform: &Affine,
) -> Vec<Path> {
    let Some(face) = crate::font::Face::parse(font_bytes) else { return Vec::new() };
    let mut out = Vec::new();
    let mut pen = x;
    for glyph in face.glyphs(&span.text) {
        if let Some(outline) = face.outline(glyph.id) {
            // Outlines are in em units with y down; scale to the font size, place at
            // the pen, then apply the item's transform and the view.
            let place = Affine::scaling(span.font.size, span.font.size)
                .then(Affine::translation(pen, baseline))
                .then(*transform);
            out.push(outline.transformed(&place));
        }
        pen += face.advance(glyph.id) * span.font.size;
    }
    out
}

// ---------------------------------------------------------------------------
// tiny-skia adapters
// ---------------------------------------------------------------------------

fn to_skia_path(path: &Path, close_subpaths: bool) -> Option<tiny_skia::Path> {
    let mut builder = PathBuilder::new();
    for sub in &path.subpaths {
        if sub.segments.is_empty() {
            continue;
        }
        builder.move_to(sub.start.x as f32, sub.start.y as f32);
        for segment in &sub.segments {
            match *segment {
                Segment::Line { to } => builder.line_to(to.x as f32, to.y as f32),
                Segment::Quadratic { ctrl, to } => {
                    builder.quad_to(ctrl.x as f32, ctrl.y as f32, to.x as f32, to.y as f32)
                }
                Segment::Cubic { ctrl1, ctrl2, to } => builder.cubic_to(
                    ctrl1.x as f32,
                    ctrl1.y as f32,
                    ctrl2.x as f32,
                    ctrl2.y as f32,
                    to.x as f32,
                    to.y as f32,
                ),
            }
        }
        // Fills behave as if closed whatever the flag says; a stroke must not, or
        // every open ink stroke would grow a closing edge.
        if sub.closed || close_subpaths {
            builder.close();
        }
    }
    builder.finish()
}

fn to_skia_stroke(stroke: &Stroke) -> SkStroke {
    SkStroke {
        width: stroke.width.max(f64::EPSILON) as f32,
        line_cap: match stroke.cap {
            LineCap::Butt => tiny_skia::LineCap::Butt,
            LineCap::Round => tiny_skia::LineCap::Round,
            LineCap::Square => tiny_skia::LineCap::Square,
        },
        line_join: match stroke.join {
            LineJoin::Miter => tiny_skia::LineJoin::Miter,
            LineJoin::Round => tiny_skia::LineJoin::Round,
            LineJoin::Bevel => tiny_skia::LineJoin::Bevel,
        },
        dash: dash_of(&stroke.dash, stroke.width),
        ..SkStroke::default()
    }
}

fn dash_of(dash: &Dash, width: f64) -> Option<StrokeDash> {
    let pattern = dash.pattern(width)?;
    StrokeDash::new(pattern.iter().map(|v| *v as f32).collect(), 0.0)
}

fn solid(color: Color, opacity: f32) -> Paint<'static> {
    let alpha = f32::from(color.a) / 255.0 * opacity.clamp(0.0, 1.0);
    let mut shader_color = tiny_skia::Color::from_rgba8(color.r, color.g, color.b, 255);
    shader_color.apply_opacity(alpha);
    Paint { shader: Shader::SolidColor(shader_color), anti_alias: true, ..Paint::default() }
}

fn to_skia_color(color: Color) -> tiny_skia::Color {
    tiny_skia::Color::from_rgba8(color.r, color.g, color.b, color.a)
}

fn transform_rect(rect: Rect, transform: &Affine) -> Rect {
    Rect::of_points(rect.corners().map(|c| transform.apply(c))).unwrap_or(rect)
}

/// tiny-skia stores premultiplied pixels; the public [`Raster`] is straight RGBA
/// because that is what every image API and every GPU upload path expects.
fn demultiply(pixmap: &Pixmap) -> Vec<u8> {
    let mut out = Vec::with_capacity(pixmap.pixels().len() * 4);
    for pixel in pixmap.pixels() {
        let c = pixel.demultiply();
        out.extend_from_slice(&[c.red(), c.green(), c.blue(), c.alpha()]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geom::{SubPath, pt};
    use crate::item::{Geometry, Item, ItemId, Kind};
    use crate::source::{Scope, Snapshot};
    use crate::style::{Style, Stroke};

    fn scene_of(items: Vec<Item>) -> Scene {
        Scene::collect(&Snapshot::new(items), &Scope::Board).expect("content")
    }

    fn red_square() -> Scene {
        scene_of(vec![
            Item::new(1, Kind::Sticky, Geometry::rect(Rect::new(0.0, 0.0, 10.0, 10.0)))
                .with_style(Style::filled(Color::rgb(255, 0, 0))),
        ])
    }

    fn raster(scene: &Scene, request: &RasterRequest) -> Raster {
        CpuRasteriser.rasterise(scene, request).expect("a raster")
    }

    #[test]
    fn scale_multiplies_the_pixel_size() {
        let scene = red_square();
        assert_eq!(raster(&scene, &RasterRequest::scene(&scene, 1.0)).width, 10);
        assert_eq!(raster(&scene, &RasterRequest::scene(&scene, 4.0)).width, 40);
    }

    #[test]
    fn a_fractional_edge_is_rounded_up_not_cropped() {
        let scene = scene_of(vec![
            Item::new(1, Kind::Sticky, Geometry::rect(Rect::new(0.0, 0.0, 10.5, 10.5)))
                .with_style(Style::filled(Color::BLACK)),
        ]);
        assert_eq!(RasterRequest::scene(&scene, 1.0).pixel_size(), (11, 11));
    }

    #[test]
    fn a_fill_lands_where_it_was_asked_to() {
        let scene = red_square();
        let raster = raster(&scene, &RasterRequest::scene(&scene, 2.0));
        assert_eq!(raster.pixel(10, 10), Some(Color::rgb(255, 0, 0)));
        assert!(raster.warnings.is_empty(), "{:?}", raster.warnings);
    }

    /// A sticky's corners are rounded on the raster too, not only in the SVG.
    #[test]
    fn a_stickys_corner_is_rounded_on_the_raster() {
        let scene = scene_of(vec![
            Item::new(1, Kind::Sticky, Geometry::rect(Rect::new(0.0, 0.0, 100.0, 100.0)))
                .with_style(Style::filled(Color::rgb(255, 0, 0))),
        ]);
        let raster = raster(&scene, &RasterRequest::scene(&scene, 1.0));
        assert_eq!(raster.pixel(0, 0).map(|c| c.a), Some(0), "the corner is cut away");
        assert_eq!(raster.pixel(50, 2), Some(Color::rgb(255, 0, 0)), "the edge is not");
    }

    #[test]
    fn no_background_leaves_transparent_pixels() {
        let scene = scene_of(vec![
            Item::new(1, Kind::Sticky, Geometry::rect(Rect::new(0.0, 0.0, 4.0, 4.0))),
            Item::new(2, Kind::Sticky, Geometry::rect(Rect::new(20.0, 20.0, 4.0, 4.0)))
                .with_style(Style::filled(Color::BLACK)),
        ]);
        let raster = raster(&scene, &RasterRequest::scene(&scene, 1.0));
        assert_eq!(raster.pixel(10, 10).map(|c| c.a), Some(0));
    }

    #[test]
    fn a_background_fills_every_pixel() {
        let scene = red_square();
        let request = RasterRequest::scene(&scene, 1.0).with_background(Color::WHITE);
        let raster = raster(&scene, &request);
        assert_eq!(raster.pixel(5, 5), Some(Color::rgb(255, 0, 0)));
        assert!(raster.rgba.chunks_exact(4).all(|p| p[3] == 255), "no pixel is left translucent");
    }

    #[test]
    fn z_order_decides_which_colour_survives() {
        let over = |first: Color, second: Color| {
            let scene = scene_of(vec![
                Item::new(1, Kind::Sticky, Geometry::rect(Rect::new(0.0, 0.0, 10.0, 10.0)))
                    .with_style(Style::filled(first))
                    .with_z(crate::item::ZOrder::from_index(0)),
                Item::new(2, Kind::Sticky, Geometry::rect(Rect::new(0.0, 0.0, 10.0, 10.0)))
                    .with_style(Style::filled(second))
                    .with_z(crate::item::ZOrder::from_index(1)),
            ]);
            raster(&scene, &RasterRequest::scene(&scene, 1.0)).pixel(5, 5)
        };
        assert_eq!(over(Color::rgb(255, 0, 0), Color::rgb(0, 0, 255)), Some(Color::rgb(0, 0, 255)));
        assert_eq!(over(Color::rgb(0, 0, 255), Color::rgb(255, 0, 0)), Some(Color::rgb(255, 0, 0)));
    }

    #[test]
    fn a_frames_clip_stops_paint_at_its_edge() {
        let scene = scene_of(vec![
            Item::new(1, Kind::frame(0), Geometry::rect(Rect::new(0.0, 0.0, 20.0, 20.0))),
            // Straddles the frame's right edge at x = 20.
            Item::new(2, Kind::Sticky, Geometry::rect(Rect::new(10.0, 0.0, 30.0, 20.0)))
                .in_frame(ItemId(1))
                .with_style(Style::filled(Color::rgb(255, 0, 0))),
        ]);
        let raster = raster(&scene, &RasterRequest::scene(&scene, 1.0));
        assert_eq!(raster.width, 20, "the clipped item cannot widen the export");
        assert_eq!(raster.pixel(15, 10), Some(Color::rgb(255, 0, 0)));
        assert_eq!(raster.pixel(19, 10), Some(Color::rgb(255, 0, 0)));
    }

    #[test]
    fn an_open_ink_stroke_is_not_closed_by_the_rasteriser() {
        // An L-shape: if the path were closed, the diagonal would be inked too.
        let path = Path::new(vec![
            SubPath::polyline(&[pt(1.0, 1.0), pt(1.0, 19.0), pt(19.0, 19.0)], false).unwrap(),
        ]);
        let scene = scene_of(vec![
            Item::new(1, Kind::Ink, Geometry::Path(path))
                .with_style(Style::NONE.with_stroke(Stroke::ink(Color::BLACK, 2.0))),
        ]);
        let raster = raster(&scene, &RasterRequest::scene(&scene, 1.0));
        let midpoint_of_the_diagonal = raster.pixel(9, 9).expect("in range");
        assert_eq!(midpoint_of_the_diagonal.a, 0, "the closing edge must not be drawn");
    }

    #[test]
    fn an_oversized_request_is_refused_with_its_numbers() {
        let scene = red_square();
        let request = RasterRequest { scale: 100_000.0, ..RasterRequest::scene(&scene, 1.0) };
        match CpuRasteriser.rasterise(&scene, &request) {
            Err(ExportError::RasterTooLarge { width, pixels, limit, .. }) => {
                assert_eq!(width, 1_000_000);
                assert!(pixels > limit);
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn a_nonsense_scale_is_refused_rather_than_producing_one_pixel() {
        let scene = red_square();
        for scale in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            let request = RasterRequest { scale, ..RasterRequest::scene(&scene, 1.0) };
            assert!(CpuRasteriser.rasterise(&scene, &request).is_err(), "scale {scale}");
        }
    }

    #[test]
    fn text_with_no_font_is_reported_rather_than_silently_dropped() {
        let scene = scene_of(vec![
            Item::new(1, Kind::Text, Geometry::rect(Rect::new(0.0, 0.0, 100.0, 40.0))).with_text(
                TextBlock::plain("hello", crate::text::FontSpec::new("Inter", 12.0), Color::BLACK),
            ),
        ]);
        let raster = raster(&scene, &RasterRequest::scene(&scene, 1.0));
        assert_eq!(raster.warnings.len(), 1, "{:?}", raster.warnings);
        assert!(raster.warnings[0].contains("Inter"), "{:?}", raster.warnings);
    }

    #[test]
    fn a_repeated_missing_font_is_reported_once() {
        let items = (0..20)
            .map(|i| {
                Item::new(i, Kind::Sticky, Geometry::rect(Rect::new(0.0, 0.0, 50.0, 20.0)))
                    .with_text(TextBlock::plain(
                        "x",
                        crate::text::FontSpec::new("Inter", 12.0),
                        Color::BLACK,
                    ))
            })
            .collect();
        let scene = scene_of(items);
        assert_eq!(raster(&scene, &RasterRequest::scene(&scene, 1.0)).warnings.len(), 1);
    }

    #[test]
    fn an_rgba_image_is_blitted_into_its_box() {
        let pixels = vec![0, 200, 0, 255];
        let source = Snapshot::new(vec![
            Item::new(1, Kind::Image, Geometry::rect(Rect::new(0.0, 0.0, 8.0, 8.0)))
                .with_image(crate::item::ImageRef::new("k")),
        ])
        .with_image("k", ImageData::Rgba8 { width: 1, height: 1, pixels });
        let scene = Scene::collect(&source, &Scope::Board).unwrap();
        let raster = raster(&scene, &RasterRequest::scene(&scene, 1.0));
        assert_eq!(raster.pixel(4, 4), Some(Color::rgb(0, 200, 0)));
    }

    #[test]
    fn a_jpeg_cannot_be_rasterised_on_the_cpu_and_says_so() {
        let source = Snapshot::new(vec![
            Item::new(1, Kind::Image, Geometry::rect(Rect::new(0.0, 0.0, 8.0, 8.0)))
                .with_image(crate::item::ImageRef::new("k")),
        ])
        .with_image("k", ImageData::encoded("image/jpeg", vec![0xff, 0xd8, 0xff]));
        let scene = Scene::collect(&source, &Scope::Board).unwrap();
        let raster = raster(&scene, &RasterRequest::scene(&scene, 1.0));
        assert_eq!(raster.warnings.len(), 1);
        assert!(raster.warnings[0].contains("image/jpeg"), "{:?}", raster.warnings);
    }

    #[test]
    fn a_png_round_trips_through_the_encoder() {
        let scene = red_square();
        let png = write(&mut CpuRasteriser, &scene, &RasterRequest::scene(&scene, 1.0)).unwrap();
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n", "a PNG signature");
        let decoded = Pixmap::decode_png(&png).expect("our own PNG decodes");
        assert_eq!((decoded.width(), decoded.height()), (10, 10));
    }

    #[test]
    fn encoding_rejects_a_buffer_of_the_wrong_length() {
        assert!(matches!(encode_rgba_png(2, 2, &[0; 8]), Err(ExportError::Png(_))));
    }
}
