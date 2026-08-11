//! Vector PDF — one page per frame, in presentation order.
//!
//! # One page per frame
//!
//! `docs/features/README.md` §2 sequences frames as a deck and §5 gives them a
//! presentation mode; a PDF is that deck, printed. So the unit of a page is a
//! frame's rect, ordered by the frame's presentation index, and a board with no
//! frames has no pages — [`ExportError::NoFrames`] says so rather than inventing a
//! single enormous page nobody can print.
//!
//! # Coordinates are converted, not mirrored
//!
//! PDF's y axis points up and its origin is the bottom-left; board space is the
//! opposite. The tempting shortcut is a mirroring CTM (`s 0 0 -s tx ty cm`), which
//! then renders every glyph backwards unless each text object counter-mirrors
//! itself. Instead each coordinate is mapped in Rust by [`PageMap`]. Text then needs
//! no special case at all: the y flip turns "up is −y" into "up is +y", which is
//! exactly what PDF text expects, so ascenders land above the baseline for free.
//!
//! # Fonts
//!
//! When the caller supplies font bytes, the file is embedded as a `/FontFile2`
//! behind a `/Type0` composite font with `Identity-H` encoding, glyphs addressed by
//! id, a `/W` array of the advances actually used, and a `/ToUnicode` CMap so the
//! text stays selectable and searchable. When it does not, the text is written in
//! Helvetica — one of PDF's fourteen standard fonts, which every viewer has — and
//! measured with the published Helvetica metrics from [`crate::font`].
//!
//! **The embedded font is the whole file, not a subset.** Subsetting means
//! rewriting `glyf`, `loca`, `cmap` and `hmtx` and renumbering every reference, and
//! getting it slightly wrong produces a PDF that opens fine on one machine and shows
//! tofu on another. A whole Inter Regular is about 300KB, which is smaller than one
//! image on the reference board. This is a deliberate size-for-certainty trade and
//! is the crate's largest known inefficiency.

use crate::error::ExportError;
use crate::font::{Descriptor, Face};
use crate::geom::{Affine, Path, Point, Rect};
use crate::item::Kind;
use crate::scene::{Frame, Placed, Scene};
use crate::source::ImageData;
use crate::style::{Color, Style};
use crate::text::{Align, FaceKey, FontSpec, TextBlock};
use pdf_writer::types::{CidFontType, FontFlags, LineCapStyle, LineJoinStyle, SystemInfo};
use pdf_writer::{Content, Filter, Finish, Name, Pdf, Rect as PdfRect, Ref, Str, TextStr};
use std::collections::{BTreeMap, BTreeSet};

/// How each frame becomes a page.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PageSize {
    /// The page has the frame's aspect ratio, scaled so its longer side is this many
    /// points. 842pt is A4's long edge, so a landscape frame prints to A4 landscape
    /// with no scaling in the print dialog.
    FitLongestSide(f64),
    /// A fixed scale: one board unit becomes this many points, and the page is
    /// whatever size that gives. Use it when a diagram must come out at a known
    /// physical size.
    Fixed { points_per_unit: f64 },
}

impl Default for PageSize {
    fn default() -> Self {
        Self::FitLongestSide(842.0)
    }
}

#[derive(Debug, Clone, Default)]
pub struct PdfOptions {
    pub page: PageSize,
    /// Painted behind each page. `None` leaves the page transparent, which prints
    /// as white but composites correctly if the PDF is placed in another document.
    pub background: Option<Color>,
    /// Embed image bytes. Off writes the geometry and leaves images out, which is
    /// the difference between a 40MB file and a 400KB one on an image-heavy board.
    pub embed_images: bool,
    /// Document title. Falls back to the scene's title.
    pub title: Option<String>,
}

impl PdfOptions {
    pub fn new() -> Self {
        Self { embed_images: true, ..Self::default() }
    }
}

/// Writes `scene` as a PDF.
///
/// Fails when the scene has no frames: see the module docs.
pub fn write(scene: &Scene, options: &PdfOptions) -> Result<Vec<u8>, ExportError> {
    if scene.frames.is_empty() {
        return Err(ExportError::NoFrames);
    }
    Builder::new(scene, options)?.build()
}

/// Writes a PDF to a file.
pub fn write_file(
    scene: &Scene,
    options: &PdfOptions,
    path: impl AsRef<std::path::Path>,
) -> Result<(), ExportError> {
    let path = path.as_ref();
    let pdf = write(scene, options)?;
    std::fs::write(path, pdf).map_err(|e| ExportError::io(path.display(), e))
}

// ---------------------------------------------------------------------------
// Page mapping
// ---------------------------------------------------------------------------

/// Board coordinates to one page's coordinates.
#[derive(Debug, Clone, Copy)]
struct PageMap {
    transform: Affine,
    width: f64,
    height: f64,
}

impl PageMap {
    fn new(frame: Rect, size: PageSize) -> Self {
        let scale = match size {
            PageSize::Fixed { points_per_unit } => points_per_unit.max(f64::EPSILON),
            PageSize::FitLongestSide(points) => {
                let longest = frame.width.max(frame.height);
                if longest > 0.0 { points / longest } else { 1.0 }
            }
        };
        // Shift the frame's top-left to the origin, scale, then flip y about the
        // page height so the frame's bottom edge lands on y = 0.
        let transform = Affine::translation(-frame.x, -frame.y)
            .then(Affine::scaling(scale, -scale))
            .then(Affine::translation(0.0, frame.height * scale));
        Self { transform, width: frame.width * scale, height: frame.height * scale }
    }

    fn point(&self, p: Point) -> (f32, f32) {
        let mapped = self.transform.apply(p);
        (mapped.x as f32, mapped.y as f32)
    }

    /// The uniform scale factor, for stroke widths and font sizes.
    fn scale(&self) -> f64 {
        self.transform.a
    }

    fn media_box(&self) -> PdfRect {
        PdfRect::new(0.0, 0.0, self.width as f32, self.height as f32)
    }

    /// The text matrix for a run whose baseline starts at `origin` under `extra`
    /// (an item rotation, typically identity).
    ///
    /// PDF text runs along the matrix's x axis and rises along its y axis, so the
    /// y column is negated: board "up" is −y, and negating it once here is what
    /// keeps glyphs upright without a second flip.
    fn text_matrix(&self, extra: Affine, origin: Point) -> [f32; 6] {
        let full = extra.then(self.transform);
        let o = full.apply(origin);
        [full.a as f32, full.b as f32, -full.c as f32, -full.d as f32, o.x as f32, o.y as f32]
    }
}

// ---------------------------------------------------------------------------
// Document assembly
// ---------------------------------------------------------------------------

/// A font as it will appear in the PDF.
struct PdfFont {
    /// `/F0`, `/F1`, …
    name: Vec<u8>,
    /// `None` for the Helvetica fallback.
    embedded: Option<EmbeddedFont>,
}

struct EmbeddedFont {
    bytes: Vec<u8>,
    descriptor: Descriptor,
    type0: Ref,
    cid: Ref,
    descriptor_ref: Ref,
    file: Ref,
    to_unicode: Ref,
    /// Glyphs actually used, so `/W` and `/ToUnicode` stay proportional to the
    /// document rather than to the font.
    used: BTreeMap<u16, (f64, char)>,
}

struct Builder<'a> {
    scene: &'a Scene,
    options: &'a PdfOptions,
    pdf: Pdf,
    next: i32,
    fonts: BTreeMap<FaceKey, PdfFont>,
    fallback: Vec<u8>,
    fallback_ref: Ref,
    images: BTreeMap<String, (Vec<u8>, Ref)>,
    /// Distinct alpha values, each an `/ExtGState`.
    alphas: Vec<(u16, Vec<u8>, Ref)>,
}

impl<'a> Builder<'a> {
    fn new(scene: &'a Scene, options: &'a PdfOptions) -> Result<Self, ExportError> {
        let mut builder = Self {
            scene,
            options,
            pdf: Pdf::new(),
            next: 1,
            fonts: BTreeMap::new(),
            fallback: b"F0".to_vec(),
            fallback_ref: Ref::new(1),
            images: BTreeMap::new(),
            alphas: Vec::new(),
        };
        builder.fallback_ref = builder.alloc();
        builder.prepare_fonts()?;
        builder.prepare_images();
        builder.prepare_alphas();
        Ok(builder)
    }

    fn alloc(&mut self) -> Ref {
        let id = Ref::new(self.next);
        self.next += 1;
        id
    }

    fn prepare_fonts(&mut self) -> Result<(), ExportError> {
        for (index, (key, bytes)) in self.scene.faces().enumerate() {
            let face = Face::parse(bytes).ok_or_else(|| {
                ExportError::font(key.family.clone(), "not a readable TrueType or OpenType file")
            })?;
            let descriptor = face.descriptor();
            let refs = (self.alloc(), self.alloc(), self.alloc(), self.alloc(), self.alloc());
            self.fonts.insert(
                key.clone(),
                PdfFont {
                    name: format!("F{}", index + 1).into_bytes(),
                    embedded: Some(EmbeddedFont {
                        bytes: bytes.to_vec(),
                        descriptor,
                        type0: refs.0,
                        cid: refs.1,
                        descriptor_ref: refs.2,
                        file: refs.3,
                        to_unicode: refs.4,
                        used: BTreeMap::new(),
                    }),
                },
            );
        }

        // Record every glyph the document will actually use, once, so the widths
        // array and the ToUnicode map are written from fact rather than from the
        // whole font.
        let mut used: BTreeMap<FaceKey, BTreeSet<char>> = BTreeMap::new();
        for placed in &self.scene.items {
            let Some(block) = &placed.item.text else { continue };
            for line in &block.lines {
                for span in &line.spans {
                    used.entry(span.font.face_key()).or_default().extend(span.text.chars());
                }
            }
        }
        for (key, chars) in used {
            let Some(font) = self.fonts.get_mut(&key) else { continue };
            let Some(embedded) = font.embedded.as_mut() else { continue };
            let Some(face) = Face::parse(&embedded.bytes) else { continue };
            for ch in chars {
                for glyph in face.glyphs(&ch.to_string()) {
                    embedded.used.insert(glyph.id, (face.advance(glyph.id), glyph.ch));
                }
            }
        }
        Ok(())
    }

    fn prepare_images(&mut self) {
        if !self.options.embed_images {
            return;
        }
        for placed in &self.scene.items {
            let Some(reference) = &placed.item.image else { continue };
            if self.images.contains_key(&reference.0) {
                continue;
            }
            if self.scene.image(reference).is_some() {
                let id = Ref::new(self.next);
                self.next += 1;
                let name = format!("Im{}", self.images.len()).into_bytes();
                self.images.insert(reference.0.clone(), (name, id));
            }
        }
    }

    fn prepare_alphas(&mut self) {
        let mut seen: BTreeSet<u16> = BTreeSet::new();
        for placed in &self.scene.items {
            let alpha = quantised_alpha(placed.item.style.opacity);
            if alpha < 1000 {
                seen.insert(alpha);
            }
        }
        for alpha in seen {
            let id = Ref::new(self.next);
            self.next += 1;
            let name = format!("GS{alpha}").into_bytes();
            self.alphas.push((alpha, name, id));
        }
    }

    fn alpha_name(&self, opacity: f32) -> Option<&[u8]> {
        let alpha = quantised_alpha(opacity);
        self.alphas.iter().find(|(a, _, _)| *a == alpha).map(|(_, name, _)| name.as_slice())
    }

    fn font_for(&self, spec: &FontSpec) -> (&[u8], bool) {
        match self.fonts.get(&spec.face_key()) {
            Some(font) => (&font.name, font.embedded.is_some()),
            None => (&self.fallback, false),
        }
    }

    fn build(mut self) -> Result<Vec<u8>, ExportError> {
        let catalog = self.alloc();
        let page_tree = self.alloc();
        let info = self.alloc();

        let pages: Vec<(Ref, Ref, Frame)> = self
            .scene
            .frames
            .iter()
            .map(|frame| (self.alloc(), self.alloc(), frame.clone()))
            .collect();

        self.pdf.catalog(catalog).pages(page_tree);
        self.pdf.document_info(info).title(TextStr(
            self.options
                .title
                .as_deref()
                .or(self.scene.title.as_deref())
                .unwrap_or("Velm board"),
        ));
        self.pdf
            .pages(page_tree)
            .kids(pages.iter().map(|(id, _, _)| *id))
            .count(pages.len() as i32);

        for (page_id, content_id, frame) in &pages {
            let map = PageMap::new(frame.rect, self.options.page);
            let content = self.page_content(frame, &map);
            self.pdf.stream(*content_id, &content);

            let mut page = self.pdf.page(*page_id);
            page.parent(page_tree).media_box(map.media_box()).contents(*content_id);
            {
                let mut resources = page.resources();
                {
                    let mut fonts = resources.fonts();
                    fonts.pair(Name(&self.fallback), self.fallback_ref);
                    for font in self.fonts.values() {
                        if let Some(embedded) = &font.embedded {
                            fonts.pair(Name(&font.name), embedded.type0);
                        }
                    }
                    fonts.finish();
                }
                if !self.images.is_empty() {
                    let mut xobjects = resources.x_objects();
                    for (name, id) in self.images.values() {
                        xobjects.pair(Name(name), *id);
                    }
                    xobjects.finish();
                }
                if !self.alphas.is_empty() {
                    let mut states = resources.ext_g_states();
                    for (_, name, id) in &self.alphas {
                        states.pair(Name(name), *id);
                    }
                    states.finish();
                }
                resources.finish();
            }
            page.finish();
        }

        self.write_fallback_font();
        self.write_embedded_fonts();
        self.write_images();
        self.write_alphas();
        Ok(self.pdf.finish())
    }

    // -- resources ----------------------------------------------------------

    fn write_fallback_font(&mut self) {
        self.pdf
            .type1_font(self.fallback_ref)
            .base_font(Name(b"Helvetica"))
            .encoding_predefined(Name(b"WinAnsiEncoding"));
    }

    fn write_embedded_fonts(&mut self) {
        // Collected first because writing borrows `self.pdf` mutably.
        let fonts: Vec<(FaceKey, &EmbeddedFont)> = self
            .fonts
            .iter()
            .filter_map(|(k, v)| v.embedded.as_ref().map(|e| (k.clone(), e)))
            .collect();
        let plan: Vec<_> = fonts
            .into_iter()
            .map(|(key, e)| {
                (
                    key,
                    e.type0,
                    e.cid,
                    e.descriptor_ref,
                    e.file,
                    e.to_unicode,
                    e.descriptor.clone(),
                    e.bytes.clone(),
                    e.used.clone(),
                )
            })
            .collect();

        for (key, type0, cid, descriptor_ref, file, to_unicode, descriptor, bytes, used) in plan {
            let base = descriptor.postscript_name.clone().into_bytes();
            self.pdf
                .type0_font(type0)
                .base_font(Name(&base))
                // Identity-H addresses glyphs directly: no encoding table, no
                // reordering, and exactly what a shaped run already is.
                .encoding_predefined(Name(b"Identity-H"))
                .descendant_font(cid)
                .to_unicode(to_unicode);

            {
                let mut cid_font = self.pdf.cid_font(cid);
                cid_font
                    .subtype(CidFontType::Type2)
                    .base_font(Name(&base))
                    .system_info(SystemInfo {
                        registry: Str(b"Adobe"),
                        ordering: Str(b"Identity"),
                        supplement: 0,
                    })
                    .font_descriptor(descriptor_ref)
                    .default_width(0.0)
                    .cid_to_gid_map_predefined(Name(b"Identity"));
                {
                    let mut widths = cid_font.widths();
                    for (glyph, (advance, _)) in &used {
                        widths.consecutive(*glyph, [(advance * 1000.0) as f32]);
                    }
                    widths.finish();
                }
                cid_font.finish();
            }

            let mut flags = FontFlags::empty();
            // Symbolic vs non-symbolic is not cosmetic: a font marked non-symbolic
            // with no `/Encoding` makes some viewers substitute their own.
            flags.insert(FontFlags::SYMBOLIC);
            if descriptor.is_italic {
                flags.insert(FontFlags::ITALIC);
            }
            let em = |v: f64| (v * 1000.0) as f32;
            self.pdf
                .font_descriptor(descriptor_ref)
                .name(Name(&base))
                .flags(flags)
                .bbox(PdfRect::new(
                    em(descriptor.bbox[0]),
                    em(descriptor.bbox[1]),
                    em(descriptor.bbox[2]),
                    em(descriptor.bbox[3]),
                ))
                .italic_angle(descriptor.italic_angle as f32)
                .ascent(em(descriptor.ascent))
                .descent(em(descriptor.descent))
                .cap_height(em(descriptor.cap_height))
                // No stem-width table is exposed by `ttf-parser`, and the value is
                // only a hinting hint. 80 is the conventional stand-in for a
                // regular weight, 160 for bold.
                .stem_v(if key.weight >= 600 { 160.0 } else { 80.0 })
                .font_file2(file);

            self.pdf.stream(file, &bytes).pair(Name(b"Length1"), bytes.len() as i32);
            let cmap = to_unicode_cmap(&used);
            self.pdf.stream(to_unicode, &cmap);
        }
    }

    fn write_images(&mut self) {
        let plan: Vec<(String, Ref)> =
            self.images.iter().map(|(k, (_, id))| (k.clone(), *id)).collect();
        for (key, id) in plan {
            let Some(data) = self.scene.image(&crate::item::ImageRef(key.clone())).cloned() else {
                continue;
            };
            match encode_image(&data) {
                Some(EncodedImage::Jpeg { bytes, width, height }) => {
                    // JPEG goes in untouched: PDF decodes DCT natively, so there is
                    // no decode, no recompression and no generation loss.
                    self.pdf
                        .image_xobject(id, &bytes)
                        .width(width as i32)
                        .height(height as i32)
                        .color_space_name(Name(b"DeviceRGB"))
                        .bits_per_component(8)
                        .filter(Filter::DctDecode);
                }
                Some(EncodedImage::Raw { rgb, alpha, width, height }) => {
                    let mask = alpha.as_ref().map(|_| {
                        let mask_id = Ref::new(self.next);
                        self.next += 1;
                        mask_id
                    });
                    let compressed = deflate(&rgb);
                    {
                        let mut image = self.pdf.image_xobject(id, &compressed);
                        image
                            .width(width as i32)
                            .height(height as i32)
                            .color_space_name(Name(b"DeviceRGB"))
                            .bits_per_component(8)
                            .filter(Filter::FlateDecode);
                        if let Some(mask_id) = mask {
                            image.s_mask(mask_id);
                        }
                        image.finish();
                    }
                    if let (Some(mask_id), Some(alpha)) = (mask, alpha) {
                        let compressed = deflate(&alpha);
                        self.pdf
                            .image_xobject(mask_id, &compressed)
                            .width(width as i32)
                            .height(height as i32)
                            .color_space_name(Name(b"DeviceGray"))
                            .bits_per_component(8)
                            .filter(Filter::FlateDecode);
                    }
                }
                None => {}
            }
        }
    }

    fn write_alphas(&mut self) {
        let plan: Vec<(u16, Ref)> = self.alphas.iter().map(|(a, _, id)| (*a, *id)).collect();
        for (alpha, id) in plan {
            let value = f32::from(alpha) / 1000.0;
            self.pdf.ext_graphics(id).non_stroking_alpha(value).stroking_alpha(value);
        }
    }

    // -- content ------------------------------------------------------------

    fn page_content(&self, frame: &Frame, map: &PageMap) -> Vec<u8> {
        let mut content = Content::new();
        if let Some(background) = self.options.background {
            let rgb = rgb(background);
            content
                .set_fill_rgb(rgb[0], rgb[1], rgb[2])
                .rect(0.0, 0.0, map.width as f32, map.height as f32)
                .fill_nonzero();
        }
        // The frame is the page, so its rect is also the page's clip. An item
        // straddling the edge is cut here rather than relying on viewers to honour
        // the media box.
        content
            .save_state()
            .rect(0.0, 0.0, map.width as f32, map.height as f32)
            .clip_nonzero()
            .end_path();

        for placed in self.scene.items.iter().filter(|p| {
            p.item.id == frame.id || p.item.frame == Some(frame.id)
        }) {
            self.draw_item(&mut content, placed, map);
        }

        content.restore_state();
        content.finish().to_vec()
    }

    fn draw_item(&self, content: &mut Content, placed: &Placed, map: &PageMap) {
        let item = &placed.item;
        if item.style.opacity <= 0.0 {
            return;
        }
        content.save_state();
        if let Some(name) = self.alpha_name(item.style.opacity) {
            content.set_parameters(Name(name));
        }

        let transform = item.transform();
        let geometry = item.silhouette().to_path().transformed(&transform);

        match &item.kind {
            Kind::Ink | Kind::Connector { .. } => {
                stroke(content, &geometry, &item.style, map);
                if let Kind::Connector { start, end } = item.kind {
                    self.draw_end_caps(content, &geometry, start, end, &item.style, map);
                }
            }
            _ => paint(content, &geometry, &item.style, map),
        }

        if let Some(reference) = &item.image
            && let Some((name, _)) = self.images.get(&reference.0)
        {
            // A PDF image occupies the unit square with its first row at the top, so
            // the CTM alone places it: no flip, and no per-image transform beyond the
            // item's own box.
            let bounds = item.geometry.bounds();
            let (x, y) = map.point(crate::geom::pt(bounds.x, bounds.bottom()));
            let (w, h) =
                ((bounds.width * map.scale()) as f32, (bounds.height * map.scale()) as f32);
            content
                .save_state()
                .transform([w, 0.0, 0.0, h, x, y])
                .x_object(Name(name))
                .restore_state();
        }

        if let Some(block) = &item.text {
            self.draw_text(content, placed, block, transform, map);
        }
        content.restore_state();
    }

    fn draw_end_caps(
        &self,
        content: &mut Content,
        path: &Path,
        start: crate::item::EndCap,
        end: crate::item::EndCap,
        style: &Style,
        map: &PageMap,
    ) {
        use crate::item::EndCap;
        let Some((head, tail)) = path.terminals() else { return };
        let Some(line) = style.effective_stroke() else { return };
        let size = line.width * 5.0;
        let filled = Style::filled(line.color);
        for (terminal, cap) in [(head, start), (tail, end)] {
            if cap == EndCap::None {
                continue;
            }
            let shape = cap.path(terminal, size);
            paint(content, &shape, &filled, map);
        }
    }

    fn draw_text(
        &self,
        content: &mut Content,
        placed: &Placed,
        block: &TextBlock,
        transform: Affine,
        map: &PageMap,
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
            let widths: Vec<f64> = line
                .spans
                .iter()
                .map(|s| crate::font::measure(self.scene.font(&s.font), &s.text, &s.font))
                .collect();
            let total: f64 = widths.iter().sum();
            let mut x = match block.align {
                Align::Left => box_.x,
                Align::Center => box_.centre().x - total / 2.0,
                Align::Right => box_.right() - total,
            };
            let baseline = box_.y + offset + line.baseline;

            for (span, width) in line.spans.iter().zip(&widths) {
                if !span.text.trim().is_empty() {
                    let (name, embedded) = self.font_for(&span.font);
                    let rgb = rgb(span.color);
                    content.begin_text();
                    content.set_font(Name(name), (span.font.size * map.scale()) as f32);
                    content.set_fill_rgb(rgb[0], rgb[1], rgb[2]);
                    content.set_text_matrix(
                        map.text_matrix(transform, crate::geom::pt(x, baseline)),
                    );
                    content.show(Str(&self.encode_show(span, embedded)));
                    content.end_text();
                }
                x += width;
            }
        }
    }

    /// The bytes of one run: two-byte glyph ids for an embedded font, WinAnsi bytes
    /// for the Helvetica fallback.
    fn encode_show(&self, span: &crate::text::Span, embedded: bool) -> Vec<u8> {
        if embedded
            && let Some(bytes) = self.scene.font(&span.font)
            && let Some(face) = Face::parse(bytes)
        {
            return face.glyphs(&span.text).iter().flat_map(|g| g.id.to_be_bytes()).collect();
        }
        // WinAnsi is Latin-1 for the range that matters here; anything outside it
        // becomes `?` rather than a byte that would decode as some other letter.
        span.text
            .chars()
            .map(|c| if (c as u32) < 256 { c as u8 } else { b'?' })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Content-stream helpers
// ---------------------------------------------------------------------------

fn paint(content: &mut Content, path: &Path, style: &Style, map: &PageMap) {
    let fill = style.effective_fill();
    let line = style.effective_stroke();
    if fill.is_none() && line.is_none() {
        return;
    }
    if let Some(color) = fill {
        let rgb = rgb(color);
        content.set_fill_rgb(rgb[0], rgb[1], rgb[2]);
    }
    if let Some(stroke) = line {
        apply_stroke_state(content, stroke, map);
    }
    if !emit_path(content, path, map) {
        return;
    }
    match (fill.is_some(), line.is_some()) {
        (true, true) => content.fill_nonzero_and_stroke(),
        (true, false) => content.fill_nonzero(),
        (false, true) => content.stroke(),
        (false, false) => content.end_path(),
    };
}

fn stroke(content: &mut Content, path: &Path, style: &Style, map: &PageMap) {
    let Some(line) = style.effective_stroke() else { return };
    apply_stroke_state(content, line, map);
    if emit_path(content, path, map) {
        content.stroke();
    }
}

fn apply_stroke_state(content: &mut Content, stroke: &crate::style::Stroke, map: &PageMap) {
    let rgb = rgb(stroke.color);
    content.set_stroke_rgb(rgb[0], rgb[1], rgb[2]);
    content.set_line_width((stroke.width * map.scale()) as f32);
    content.set_line_cap(match stroke.cap.pdf() {
        1 => LineCapStyle::RoundCap,
        2 => LineCapStyle::ProjectingSquareCap,
        _ => LineCapStyle::ButtCap,
    });
    content.set_line_join(match stroke.join.pdf() {
        1 => LineJoinStyle::RoundJoin,
        2 => LineJoinStyle::BevelJoin,
        _ => LineJoinStyle::MiterJoin,
    });
    match stroke.dash.pattern(stroke.width) {
        Some(pattern) => {
            let scaled: Vec<f32> = pattern.iter().map(|v| (v * map.scale()) as f32).collect();
            content.set_dash_pattern(scaled, 0.0);
        }
        None => {
            content.set_dash_pattern([], 0.0);
        }
    };
}

/// Writes the path operators. Returns false when there was nothing to write, so the
/// caller does not emit a paint operator with no current path.
fn emit_path(content: &mut Content, path: &Path, map: &PageMap) -> bool {
    let mut wrote = false;
    for sub in &path.subpaths {
        if sub.segments.is_empty() {
            continue;
        }
        let (x, y) = map.point(sub.start);
        content.move_to(x, y);
        let mut cursor = sub.start;
        for segment in &sub.segments {
            match *segment {
                crate::geom::Segment::Line { to } => {
                    let (x, y) = map.point(to);
                    content.line_to(x, y);
                }
                other => {
                    // PDF has no quadratic operator, so quadratics are raised to
                    // cubics exactly rather than flattened.
                    let (c1, c2, to) = other.to_cubic(cursor);
                    let (x1, y1) = map.point(c1);
                    let (x2, y2) = map.point(c2);
                    let (x3, y3) = map.point(to);
                    content.cubic_to(x1, y1, x2, y2, x3, y3);
                }
            }
            cursor = segment.end();
        }
        if sub.closed {
            content.close_path();
        }
        wrote = true;
    }
    wrote
}

fn rgb(color: Color) -> [f32; 3] {
    [
        f32::from(color.r) / 255.0,
        f32::from(color.g) / 255.0,
        f32::from(color.b) / 255.0,
    ]
}

/// Opacity in thousandths, so near-identical values share one `/ExtGState`.
fn quantised_alpha(opacity: f32) -> u16 {
    (opacity.clamp(0.0, 1.0) * 1000.0).round() as u16
}

// ---------------------------------------------------------------------------
// Images
// ---------------------------------------------------------------------------

enum EncodedImage {
    Jpeg { bytes: Vec<u8>, width: u32, height: u32 },
    Raw { rgb: Vec<u8>, alpha: Option<Vec<u8>>, width: u32, height: u32 },
}

fn encode_image(data: &ImageData) -> Option<EncodedImage> {
    match data {
        ImageData::Encoded { media_type, bytes } if media_type == "image/jpeg" => {
            let (width, height) = jpeg_size(bytes)?;
            Some(EncodedImage::Jpeg { bytes: bytes.clone(), width, height })
        }
        ImageData::Encoded { bytes, .. } => {
            let pixmap = tiny_skia::Pixmap::decode_png(bytes).ok()?;
            let (width, height) = (pixmap.width(), pixmap.height());
            let straight: Vec<u8> = pixmap
                .pixels()
                .iter()
                .flat_map(|p| {
                    let c = p.demultiply();
                    [c.red(), c.green(), c.blue(), c.alpha()]
                })
                .collect();
            Some(split_rgba(&straight, width, height))
        }
        ImageData::Rgba8 { width, height, pixels } => {
            Some(split_rgba(pixels, *width, *height))
        }
    }
}

/// Splits straight RGBA into a colour stream and, if any pixel is translucent, an
/// alpha stream for `/SMask`. PDF has no RGBA image; transparency is a separate
/// grayscale image.
fn split_rgba(pixels: &[u8], width: u32, height: u32) -> EncodedImage {
    let mut rgb = Vec::with_capacity(pixels.len() / 4 * 3);
    let mut alpha = Vec::with_capacity(pixels.len() / 4);
    let mut translucent = false;
    for chunk in pixels.chunks_exact(4) {
        rgb.extend_from_slice(&chunk[..3]);
        alpha.push(chunk[3]);
        translucent |= chunk[3] != 255;
    }
    EncodedImage::Raw { rgb, alpha: translucent.then_some(alpha), width, height }
}

/// Pixel dimensions from a JPEG's start-of-frame marker.
///
/// Needed because a PDF image XObject must declare `/Width` and `/Height` even when
/// the bytes go in untouched — the only field the container cannot infer.
fn jpeg_size(bytes: &[u8]) -> Option<(u32, u32)> {
    let mut i = 2; // skip SOI
    while i + 9 < bytes.len() {
        if bytes[i] != 0xFF {
            i += 1;
            continue;
        }
        let marker = bytes[i + 1];
        // SOF0..SOF15, excluding the four that are not frame headers.
        if (0xC0..=0xCF).contains(&marker)
            && !matches!(marker, 0xC4 | 0xC8 | 0xCC)
        {
            let height = u32::from(u16::from_be_bytes([bytes[i + 5], bytes[i + 6]]));
            let width = u32::from(u16::from_be_bytes([bytes[i + 7], bytes[i + 8]]));
            return (width > 0 && height > 0).then_some((width, height));
        }
        let length = u16::from_be_bytes([bytes[i + 2], bytes[i + 3]]) as usize;
        if length < 2 {
            return None;
        }
        i += 2 + length;
    }
    None
}

fn deflate(bytes: &[u8]) -> Vec<u8> {
    miniz_oxide::deflate::compress_to_vec_zlib(bytes, 6)
}

// ---------------------------------------------------------------------------
// ToUnicode
// ---------------------------------------------------------------------------

/// A `/ToUnicode` CMap: glyph id back to the characters it came from.
///
/// Without it a PDF's text is a picture — not selectable, not searchable, not
/// copyable — which for a board of written notes would lose most of the point of
/// exporting it as PDF rather than PNG.
fn to_unicode_cmap(used: &BTreeMap<u16, (f64, char)>) -> Vec<u8> {
    let mut out = String::from(
        "/CIDInit /ProcSet findresource begin\n\
         12 dict begin\nbegincmap\n\
         /CIDSystemInfo << /Registry (Adobe) /Ordering (UCS) /Supplement 0 >> def\n\
         /CMapName /Adobe-Identity-UCS def\n/CMapType 2 def\n\
         1 begincodespacerange\n<0000> <FFFF>\nendcodespacerange\n",
    );
    // `bfchar` sections are capped at 100 entries by the specification, so the
    // list is chunked rather than written as one run.
    let list: Vec<(u16, char)> = used.iter().map(|(g, (_, ch))| (*g, *ch)).collect();
    for chunk in list.chunks(100) {
        out.push_str(&format!("{} beginbfchar\n", chunk.len()));
        for (glyph, ch) in chunk {
            let mut utf16 = [0u16; 2];
            let encoded = ch.encode_utf16(&mut utf16);
            let hex: String = encoded.iter().map(|u| format!("{u:04X}")).collect();
            out.push_str(&format!("<{glyph:04X}> <{hex}>\n"));
        }
        out.push_str("endbfchar\n");
    }
    out.push_str("endcmap\nCMapName currentdict /CMap defineresource pop\nend\nend\n");
    out.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geom::{SubPath, pt};
    use crate::item::{Geometry, Item, ItemId};
    use crate::source::{Scope, Snapshot};
    use crate::style::Stroke;
    use crate::text::TextBlock;

    fn frame(id: u64, order: u32, rect: Rect, name: &str) -> Item {
        Item::new(id, Kind::frame(order), Geometry::rect(rect)).with_name(name)
    }

    fn board() -> Scene {
        let items = vec![
            frame(1, 1, Rect::new(0.0, 0.0, 400.0, 200.0), "second"),
            frame(2, 0, Rect::new(500.0, 0.0, 200.0, 400.0), "first"),
            Item::new(3, Kind::Sticky, Geometry::rect(Rect::new(10.0, 10.0, 50.0, 50.0)))
                .in_frame(ItemId(1))
                .with_style(Style::filled(Color::from_hex("#fff79e").unwrap())),
        ];
        Scene::collect(&Snapshot::new(items), &Scope::Board).expect("content")
    }

    fn pdf_of(scene: &Scene) -> Vec<u8> {
        write(scene, &PdfOptions::new()).expect("a PDF")
    }

    fn text_of(pdf: &[u8]) -> String {
        String::from_utf8_lossy(pdf).into_owned()
    }

    #[test]
    fn a_board_with_no_frames_has_no_pages() {
        let items = vec![Item::new(
            1,
            Kind::Sticky,
            Geometry::rect(Rect::new(0.0, 0.0, 10.0, 10.0)),
        )];
        let scene = Scene::collect(&Snapshot::new(items), &Scope::Board).unwrap();
        assert!(matches!(write(&scene, &PdfOptions::new()), Err(ExportError::NoFrames)));
    }

    #[test]
    fn the_file_is_a_pdf_with_a_trailer() {
        let pdf = pdf_of(&board());
        assert_eq!(&pdf[..5], b"%PDF-");
        let tail = text_of(&pdf);
        assert!(tail.contains("trailer"), "no trailer");
        assert!(tail.trim_end().ends_with("%%EOF"), "no EOF marker");
    }

    #[test]
    fn there_is_one_page_per_frame_in_presentation_order() {
        let pdf = text_of(&pdf_of(&board()));
        assert!(pdf.contains("/Count 2"), "{pdf}");
        // The first page is the portrait frame (order 0, 200×400), the second the
        // landscape one — so the media boxes appear in that order.
        let first = pdf.find("/MediaBox [0 0 421 842]");
        let second = pdf.find("/MediaBox [0 0 842 421]");
        assert!(first.is_some() && second.is_some(), "{pdf}");
        assert!(first < second, "presentation order decides page order: {pdf}");
    }

    #[test]
    fn fit_longest_side_preserves_the_frames_aspect() {
        let scene = board();
        let map = PageMap::new(scene.frames[1].rect, PageSize::FitLongestSide(842.0));
        assert!((map.width - 842.0).abs() < 1e-6);
        assert!((map.height - 421.0).abs() < 1e-6, "{}", map.height);
    }

    #[test]
    fn a_fixed_scale_gives_a_known_physical_size() {
        let map = PageMap::new(
            Rect::new(0.0, 0.0, 100.0, 50.0),
            PageSize::Fixed { points_per_unit: 0.75 },
        );
        assert!((map.width - 75.0).abs() < 1e-9);
        assert!((map.height - 37.5).abs() < 1e-9);
    }

    /// The whole reason coordinates are converted rather than mirrored.
    #[test]
    fn the_y_axis_is_flipped_so_the_frames_top_is_the_pages_top() {
        let map = PageMap::new(Rect::new(0.0, 0.0, 100.0, 100.0), PageSize::Fixed { points_per_unit: 1.0 });
        assert_eq!(map.point(pt(0.0, 0.0)), (0.0, 100.0), "board top-left is page top-left");
        assert_eq!(map.point(pt(0.0, 100.0)), (0.0, 0.0), "board bottom-left is the origin");
        assert_eq!(map.point(pt(100.0, 100.0)), (100.0, 0.0));
    }

    #[test]
    fn text_is_upright_under_the_flip() {
        let map = PageMap::new(Rect::new(0.0, 0.0, 100.0, 100.0), PageSize::Fixed { points_per_unit: 1.0 });
        let m = map.text_matrix(Affine::IDENTITY, pt(10.0, 40.0));
        // A positive, unmirrored y scale is what keeps glyphs the right way up.
        assert_eq!([m[0], m[1], m[2], m[3]], [1.0, 0.0, 0.0, 1.0]);
        assert_eq!([m[4], m[5]], [10.0, 60.0]);
    }

    #[test]
    fn a_rotated_item_gets_a_rotated_text_matrix() {
        let map = PageMap::new(Rect::new(0.0, 0.0, 100.0, 100.0), PageSize::Fixed { points_per_unit: 1.0 });
        let rotation = Affine::rotation_about(90.0, pt(50.0, 50.0));
        let m = map.text_matrix(rotation, pt(50.0, 50.0));
        assert!((m[0]).abs() < 1e-6 && (m[1] + 1.0).abs() < 1e-6, "{m:?}");
    }

    #[test]
    fn only_a_frames_own_items_reach_its_page() {
        let scene = board();
        let map = PageMap::new(scene.frames[0].rect, PageSize::default());
        let content = Builder::new(&scene, &PdfOptions::new())
            .unwrap()
            .page_content(&scene.frames[0], &map);
        // Frame 2 has no children, so its page paints only the frame itself.
        let ops = String::from_utf8_lossy(&content);
        assert!(!ops.contains("1 0.968627 0.619608 rg"), "the sticky belongs to the other page");
    }

    #[test]
    fn a_sticky_is_filled_with_its_own_colour() {
        let scene = board();
        let map = PageMap::new(scene.frames[1].rect, PageSize::default());
        let content = Builder::new(&scene, &PdfOptions::new())
            .unwrap()
            .page_content(&scene.frames[1], &map);
        let ops = String::from_utf8_lossy(&content);
        assert!(ops.contains(" rg"), "a fill colour was set: {ops}");
        assert!(ops.contains("f\n") || ops.contains(" f"), "something was filled: {ops}");
    }

    #[test]
    fn dashes_and_caps_reach_the_content_stream() {
        let path = Path::new(vec![
            SubPath::polyline(&[pt(10.0, 10.0), pt(90.0, 90.0)], false).unwrap(),
        ]);
        let items = vec![
            frame(1, 0, Rect::new(0.0, 0.0, 100.0, 100.0), "f"),
            Item::new(2, Kind::Ink, Geometry::Path(path))
                .in_frame(ItemId(1))
                .with_style(Style::NONE.with_stroke(
                    Stroke::ink(Color::BLACK, 2.0).with_dash(crate::style::Dash::Dashed),
                )),
        ];
        let scene = Scene::collect(&Snapshot::new(items), &Scope::Board).unwrap();
        let map = PageMap::new(scene.frames[0].rect, PageSize::default());
        let ops = String::from_utf8_lossy(
            &Builder::new(&scene, &PdfOptions::new()).unwrap().page_content(&scene.frames[0], &map),
        )
        .into_owned();
        assert!(ops.contains(" d\n") || ops.contains("] 0 d"), "a dash array: {ops}");
        assert!(ops.contains("1 J"), "round caps: {ops}");
        assert!(ops.contains("S"), "a stroke: {ops}");
    }

    #[test]
    fn without_font_bytes_the_text_falls_back_to_helvetica() {
        let items = vec![
            frame(1, 0, Rect::new(0.0, 0.0, 200.0, 100.0), "f"),
            Item::new(2, Kind::Text, Geometry::rect(Rect::new(0.0, 0.0, 200.0, 100.0)))
                .in_frame(ItemId(1))
                .with_text(TextBlock::plain("Hello", FontSpec::new("Inter", 14.0), Color::BLACK)),
        ];
        let scene = Scene::collect(&Snapshot::new(items), &Scope::Board).unwrap();
        let pdf = text_of(&pdf_of(&scene));
        assert!(pdf.contains("/BaseFont /Helvetica"), "{pdf}");
        assert!(pdf.contains("(Hello)"), "the fallback writes literal text: {pdf}");
    }

    #[test]
    fn the_document_title_comes_from_the_board() {
        let scene = board();
        let pdf = text_of(&write(&scene, &PdfOptions::new()).unwrap());
        assert!(pdf.contains("/Title"), "{pdf}");
    }

    #[test]
    fn item_opacity_becomes_an_ext_graphics_state() {
        let items = vec![
            frame(1, 0, Rect::new(0.0, 0.0, 100.0, 100.0), "f"),
            Item::new(2, Kind::Sticky, Geometry::rect(Rect::new(10.0, 10.0, 20.0, 20.0)))
                .in_frame(ItemId(1))
                .with_style(Style::filled(Color::BLACK).with_opacity(0.5)),
        ];
        let scene = Scene::collect(&Snapshot::new(items), &Scope::Board).unwrap();
        let pdf = text_of(&pdf_of(&scene));
        assert!(pdf.contains("/CA 0.5") && pdf.contains("/ca 0.5"), "{pdf}");
    }

    #[test]
    fn a_jpeg_is_embedded_untouched_behind_a_dct_filter() {
        // A minimal JPEG header: SOI, then a SOF0 declaring 4×3.
        let jpeg = vec![
            0xFF, 0xD8, 0xFF, 0xC0, 0x00, 0x11, 0x08, 0x00, 0x03, 0x00, 0x04, 0x03, 0x01, 0x11,
            0x00, 0x02, 0x11, 0x01, 0x03, 0x11, 0x01,
        ];
        assert_eq!(jpeg_size(&jpeg), Some((4, 3)));

        let items = vec![
            frame(1, 0, Rect::new(0.0, 0.0, 100.0, 100.0), "f"),
            Item::new(2, Kind::Image, Geometry::rect(Rect::new(10.0, 10.0, 40.0, 30.0)))
                .in_frame(ItemId(1))
                .with_image(crate::item::ImageRef::new("k")),
        ];
        let source = Snapshot::new(items).with_image("k", ImageData::encoded("image/jpeg", jpeg));
        let scene = Scene::collect(&source, &Scope::Board).unwrap();
        let pdf = text_of(&pdf_of(&scene));
        assert!(pdf.contains("/DCTDecode"), "{pdf}");
        assert!(pdf.contains("/Width 4") && pdf.contains("/Height 3"), "{pdf}");
    }

    #[test]
    fn a_translucent_raw_image_gets_a_soft_mask() {
        let items = vec![
            frame(1, 0, Rect::new(0.0, 0.0, 100.0, 100.0), "f"),
            Item::new(2, Kind::Image, Geometry::rect(Rect::new(10.0, 10.0, 40.0, 30.0)))
                .in_frame(ItemId(1))
                .with_image(crate::item::ImageRef::new("k")),
        ];
        let source = Snapshot::new(items).with_image(
            "k",
            ImageData::Rgba8 { width: 2, height: 1, pixels: vec![255, 0, 0, 255, 0, 255, 0, 128] },
        );
        let scene = Scene::collect(&source, &Scope::Board).unwrap();
        let pdf = text_of(&pdf_of(&scene));
        assert!(pdf.contains("/SMask"), "{pdf}");
        assert!(pdf.contains("/DeviceGray"), "{pdf}");
    }

    #[test]
    fn an_opaque_image_gets_no_soft_mask() {
        let items = vec![
            frame(1, 0, Rect::new(0.0, 0.0, 100.0, 100.0), "f"),
            Item::new(2, Kind::Image, Geometry::rect(Rect::new(10.0, 10.0, 40.0, 30.0)))
                .in_frame(ItemId(1))
                .with_image(crate::item::ImageRef::new("k")),
        ];
        let source = Snapshot::new(items).with_image(
            "k",
            ImageData::Rgba8 { width: 1, height: 1, pixels: vec![9, 9, 9, 255] },
        );
        let scene = Scene::collect(&source, &Scope::Board).unwrap();
        assert!(!text_of(&pdf_of(&scene)).contains("/SMask"));
    }

    #[test]
    fn to_unicode_maps_every_glyph_back_to_its_character() {
        let used = BTreeMap::from([(36u16, (0.5, 'A')), (37, (0.5, 'B'))]);
        let cmap = String::from_utf8(to_unicode_cmap(&used)).unwrap();
        assert!(cmap.contains("2 beginbfchar"), "{cmap}");
        assert!(cmap.contains("<0024> <0041>"), "{cmap}");
        assert!(cmap.contains("<0025> <0042>"), "{cmap}");
    }

    #[test]
    fn to_unicode_splits_at_the_hundred_entry_limit() {
        let used: BTreeMap<u16, (f64, char)> =
            (0..250u16).map(|i| (i, (0.5, char::from(b'a' + (i % 26) as u8)))).collect();
        let cmap = String::from_utf8(to_unicode_cmap(&used)).unwrap();
        assert_eq!(cmap.matches("beginbfchar").count(), 3, "100 + 100 + 50");
        assert!(cmap.contains("50 beginbfchar"), "{cmap}");
    }

    #[test]
    fn a_malformed_jpeg_is_reported_as_unsized_rather_than_panicking() {
        assert_eq!(jpeg_size(&[0xFF, 0xD8]), None);
        assert_eq!(jpeg_size(&[]), None);
        assert_eq!(jpeg_size(&[0xFF, 0xD8, 0xFF, 0xC0, 0x00, 0x00]), None);
    }
}
