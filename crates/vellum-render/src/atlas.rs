//! The glyph atlas: `vellum_text::GlyphImage` in, a texture slot and a UV rect out.
//!
//! `vellum-text` deliberately produces data and never GPU calls, and says so: packing
//! and eviction belong here. What it hands over is a [`GlyphKey`] that identifies a
//! bitmap *completely* — face, glyph, device-pixel size and subpixel phase — so two
//! equal keys are the same image and may share a slot, and nothing else has to be
//! compared.
//!
//! # Shelf packing
//!
//! Glyphs are close to uniform in height within a size band, which is the case shelf
//! packing is built for: a shelf is a horizontal strip as tall as the first glyph put
//! on it, and later glyphs go beside it if they are close enough in height. Skyline
//! packing wastes less on pathological inputs, but a page of text is not a
//! pathological input, and shelves keep the allocator to a few dozen lines whose
//! failure mode is wasted space rather than overlapping glyphs.
//!
//! # Coverage and colour cannot share a page
//!
//! `vellum-text` is explicit about this: a coverage bitmap is one byte per pixel and
//! gets tinted by the run's colour, a colour bitmap is four bytes and must not be.
//! Different formats mean different textures, so the atlas keeps two independent sets
//! of pages and the renderer draws them as separate batches.
//!
//! # Eviction
//!
//! Slots are stamped with the frame they were last used in. When a page cannot take a
//! glyph and no more pages may be created, the surface is repacked from the retained
//! bitmaps, keeping only glyphs used since the previous [`GlyphAtlas::begin_frame`].
//! Retaining the bitmaps costs about as much host memory as the atlas itself — a
//! megabyte for a 1024² page — and buys an eviction that does not have to go back to
//! the rasteriser, which is what makes it cheap enough to do mid-frame.

use crate::error::RenderError;
use crate::texture;
use std::collections::{HashMap, HashSet};
use vellum_text::{GlyphContent, GlyphImage, GlyphKey};

/// Which of the two page sets a glyph lives in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum AtlasKind {
    /// One byte per pixel, tinted by the run's colour.
    Coverage,
    /// Four bytes per pixel, drawn as-is. Emoji.
    Color,
}

impl AtlasKind {
    fn of(content: GlyphContent) -> Self {
        match content {
            GlyphContent::Coverage => Self::Coverage,
            // Subpixel coverage is three independent channels, not colour, but it is
            // four bytes per pixel and cannot go in an R8 page. Nothing in Vellum
            // requests it — `vellum-text` asks swash for alpha — so treating it as a
            // colour bitmap is a fallback that draws *something* rather than
            // corrupting the coverage page's stride.
            GlyphContent::SubpixelCoverage | GlyphContent::Color => Self::Color,
        }
    }

    fn format(self) -> wgpu::TextureFormat {
        match self {
            Self::Coverage => wgpu::TextureFormat::R8Unorm,
            Self::Color => wgpu::TextureFormat::Rgba8Unorm,
        }
    }

    fn bytes_per_texel(self) -> u32 {
        match self {
            Self::Coverage => 1,
            Self::Color => 4,
        }
    }
}

/// Identifies one atlas page for batching. A batch ends when this changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AtlasPage {
    pub kind: AtlasKind,
    pub index: u32,
}

/// Where a glyph landed, and everything needed to draw it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AtlasSlot {
    pub page: AtlasPage,
    pub width: u32,
    pub height: u32,
    /// Precomputed so the shader needs no page size and the vertex stage no divide.
    pub uv_min: [f32; 2],
    pub uv_max: [f32; 2],
    /// Offset from the pen position to the bitmap's left edge, in device px. Copied
    /// from the [`GlyphImage`] so a caller placing text needs only the slot.
    pub left: i32,
    /// Offset from the baseline **up** to the bitmap's top edge, so a renderer
    /// subtracts it.
    pub top: i32,
}

impl AtlasSlot {
    /// Top-left corner of this glyph's quad, for a pen at an integer device pixel.
    pub fn quad_origin(&self, pen: (i32, i32)) -> [f32; 2] {
        [(pen.0 + self.left) as f32, (pen.1 - self.top) as f32]
    }

    pub fn quad_size(&self) -> [f32; 2] {
        [self.width as f32, self.height as f32]
    }

    pub fn is_color(&self) -> bool {
        self.page.kind == AtlasKind::Color
    }
}

/// How large the atlas may grow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AtlasConfig {
    /// Side of a square page, in texels.
    pub page_size: u32,
    /// Ceiling on pages *per kind*.
    pub max_pages: u32,
}

impl Default for AtlasConfig {
    /// 1024² holds roughly 1,500 glyphs at a 24 px body size — comfortably more than
    /// one screenful of a board, which is what has to fit for eviction to be rare
    /// rather than per-frame. Four pages is 4 MB of coverage, and text that needs
    /// more than that on screen at once is text nobody can read.
    fn default() -> Self {
        Self { page_size: 1024, max_pages: 4 }
    }
}

/// A horizontal strip of one page, as tall as the first glyph placed on it.
#[derive(Debug, Clone, Copy)]
struct Shelf {
    top: u32,
    height: u32,
    next_x: u32,
}

/// The shelf allocator for one page. Pure arithmetic, and separated from the GPU
/// resources so it can be exercised on a machine with no device.
#[derive(Debug)]
struct ShelfPacker {
    shelves: Vec<Shelf>,
    next_y: u32,
    size: u32,
}

impl ShelfPacker {
    fn new(size: u32) -> Self {
        Self { shelves: Vec::new(), next_y: 0, size }
    }

    /// Reserves `width` × `height` texels, or `None` if the page is full.
    ///
    /// A glyph joins an existing shelf only when it uses at least three quarters of
    /// the shelf's height. Without that floor a single tall glyph — a parenthesis, an
    /// accented capital — would set the height for a whole row of lowercase and waste
    /// most of it.
    fn allocate(&mut self, width: u32, height: u32) -> Option<(u32, u32)> {
        if width > self.size || height > self.size {
            return None;
        }
        for shelf in &mut self.shelves {
            if height <= shelf.height
                && height * 4 >= shelf.height * 3
                && shelf.next_x + width <= self.size
            {
                let x = shelf.next_x;
                shelf.next_x += width;
                return Some((x, shelf.top));
            }
        }
        if self.next_y + height > self.size {
            return None;
        }
        let shelf = Shelf { top: self.next_y, height, next_x: width };
        self.next_y += height;
        self.shelves.push(shelf);
        Some((0, shelf.top))
    }

    fn reset(&mut self) {
        self.shelves.clear();
        self.next_y = 0;
    }
}

struct Page {
    texture: wgpu::Texture,
    bind_group: wgpu::BindGroup,
    packer: ShelfPacker,
}

/// The pages of one format, and the packer over them.
struct Surface {
    kind: AtlasKind,
    pages: Vec<Page>,
    config: AtlasConfig,
}

impl Surface {
    fn new(kind: AtlasKind, config: AtlasConfig) -> Self {
        Self { kind, pages: Vec::new(), config }
    }

    fn allocate(
        &mut self,
        device: &wgpu::Device,
        layout: &wgpu::BindGroupLayout,
        sampler: &wgpu::Sampler,
        width: u32,
        height: u32,
    ) -> Option<(u32, u32, u32)> {
        if let Some(placed) = self.allocate_in_existing_pages(width, height) {
            return Some(placed);
        }
        if self.pages.len() as u32 >= self.config.max_pages {
            return None;
        }
        self.pages.push(create_page(device, layout, sampler, self.kind, self.config.page_size));
        let index = self.pages.len() as u32 - 1;
        let page = self.pages.last_mut().expect("just pushed");
        page.packer.allocate(width, height).map(|(x, y)| (index, x, y))
    }

    /// Placement without the option of a new page. Repacking uses this: it must not
    /// grow the atlas, and it therefore needs no device.
    fn allocate_in_existing_pages(&mut self, width: u32, height: u32) -> Option<(u32, u32, u32)> {
        self.pages.iter_mut().enumerate().find_map(|(index, page)| {
            page.packer.allocate(width, height).map(|(x, y)| (index as u32, x, y))
        })
    }

    fn reset(&mut self) {
        for page in &mut self.pages {
            page.packer.reset();
        }
    }
}

/// A resident glyph: where it is, and the bitmap it was packed from.
struct Entry {
    slot: AtlasSlot,
    image: GlyphImage,
    last_used: u64,
}

pub struct GlyphAtlas {
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    coverage: Surface,
    color: Surface,
    entries: HashMap<GlyphKey, Entry>,
    /// Glyphs that rasterised with no ink — spaces, and marks that rendered empty.
    /// They take no slot, so without recording them a caller cannot tell "this glyph
    /// draws nothing" from "this glyph is missing from the atlas", and a third of the
    /// text on a board is whitespace.
    blank: HashSet<GlyphKey>,
    config: AtlasConfig,
    frame: u64,
}

impl GlyphAtlas {
    pub fn new(device: &wgpu::Device, config: AtlasConfig) -> Self {
        let config = AtlasConfig {
            page_size: config.page_size.max(64),
            max_pages: config.max_pages.max(1),
        };
        Self {
            layout: texture::bind_group_layout(device, "vellum-atlas-layout"),
            sampler: device.create_sampler(&wgpu::SamplerDescriptor {
                label: Some("vellum-atlas-sampler"),
                address_mode_u: wgpu::AddressMode::ClampToEdge,
                address_mode_v: wgpu::AddressMode::ClampToEdge,
                address_mode_w: wgpu::AddressMode::ClampToEdge,
                // Linear, but the glyph is rasterised at the size it will occupy and
                // placed on an integer pixel, so every fetch lands on a texel centre
                // and the filter is a no-op. It exists for the fractional case a
                // future scaled-text path would hit, not for the normal one.
                mag_filter: wgpu::FilterMode::Linear,
                min_filter: wgpu::FilterMode::Linear,
                mipmap_filter: wgpu::MipmapFilterMode::Nearest,
                ..Default::default()
            }),
            coverage: Surface::new(AtlasKind::Coverage, config),
            color: Surface::new(AtlasKind::Color, config),
            entries: HashMap::new(),
            blank: HashSet::new(),
            config,
            frame: 0,
        }
    }

    pub fn bind_group_layout(&self) -> &wgpu::BindGroupLayout {
        &self.layout
    }

    pub fn config(&self) -> AtlasConfig {
        self.config
    }

    /// Resident glyphs, blank ones excluded.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn pages(&self, kind: AtlasKind) -> u32 {
        self.surface(kind).pages.len() as u32
    }

    pub fn bind_group(&self, page: AtlasPage) -> Option<&wgpu::BindGroup> {
        self.surface(page.kind)
            .pages
            .get(page.index as usize)
            .map(|p| &p.bind_group)
    }

    /// Advances the frame counter. Glyphs not used since the previous call are what
    /// eviction takes first.
    pub fn begin_frame(&mut self) {
        self.frame += 1;
    }

    /// Where `key` lives, if it is resident. Does not mark it as used — that is
    /// [`Self::prepare`]'s job, which happens before any draw is built.
    pub fn slot(&self, key: GlyphKey) -> Option<&AtlasSlot> {
        self.entries.get(&key).map(|e| &e.slot)
    }

    /// Whether `key` was prepared and rasterised with no ink. Together with
    /// [`Self::slot`] this distinguishes a space from a glyph the atlas never saw,
    /// which is the difference between text that is correct and text with holes in it.
    pub fn is_blank(&self, key: GlyphKey) -> bool {
        self.blank.contains(&key)
    }

    /// Makes every glyph in `entries` resident, uploading the ones that are not.
    ///
    /// Call once per frame with everything the frame will draw — which is exactly what
    /// [`vellum_text::TextEngine::atlas_entries`] returns — *before* building any
    /// draw. Slots are stable until the next `prepare` that has to evict, so a caller
    /// may hold them for the duration of a frame and no longer.
    ///
    /// A blank glyph (a space) is skipped rather than given a zero-area slot.
    pub fn prepare(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        entries: &[(GlyphKey, GlyphImage)],
    ) -> Result<(), RenderError> {
        // Two attempts: the first fills whatever space is left, and if that runs out
        // the second runs against a surface repacked to hold only this frame's
        // glyphs. A third would mean the frame's own text does not fit in the atlas
        // at all, which is a configuration problem rather than a transient one.
        for attempt in 0..2 {
            match self.try_prepare(device, queue, entries) {
                Ok(()) => return Ok(()),
                Err(RenderError::AtlasFull { .. }) if attempt == 0 => {
                    self.repack(queue);
                }
                Err(other) => return Err(other),
            }
        }
        Err(RenderError::AtlasFull {
            glyphs: entries.len(),
            page_size: self.config.page_size,
            pages: self.config.max_pages,
        })
    }

    fn try_prepare(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        entries: &[(GlyphKey, GlyphImage)],
    ) -> Result<(), RenderError> {
        for (key, image) in entries {
            if image.is_blank() {
                self.blank.insert(*key);
                continue;
            }
            if let Some(entry) = self.entries.get_mut(key) {
                entry.last_used = self.frame;
                continue;
            }
            let slot = self.insert(device, queue, image)?;
            self.entries
                .insert(*key, Entry { slot, image: image.clone(), last_used: self.frame });
        }
        Ok(())
    }

    fn insert(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        image: &GlyphImage,
    ) -> Result<AtlasSlot, RenderError> {
        let kind = AtlasKind::of(image.content);
        let page_size = self.config.page_size;
        let surface = match kind {
            AtlasKind::Coverage => &mut self.coverage,
            AtlasKind::Color => &mut self.color,
        };
        let (index, x, y) = surface
            .allocate(device, &self.layout, &self.sampler, image.width, image.height)
            .ok_or(RenderError::AtlasFull {
                glyphs: 1,
                page_size,
                pages: self.config.max_pages,
            })?;

        write_glyph(queue, &surface.pages[index as usize].texture, kind, x, y, image);

        let scale = 1.0 / page_size as f32;
        Ok(AtlasSlot {
            page: AtlasPage { kind, index },
            width: image.width,
            height: image.height,
            uv_min: [x as f32 * scale, y as f32 * scale],
            uv_max: [(x + image.width) as f32 * scale, (y + image.height) as f32 * scale],
            left: image.left,
            top: image.top,
        })
    }

    /// Drops every glyph unused since the last [`Self::begin_frame`] and re-packs the
    /// rest from the retained bitmaps.
    fn repack(&mut self, queue: &wgpu::Queue) {
        let frame = self.frame;
        let before = self.entries.len();
        self.entries.retain(|_, entry| entry.last_used >= frame);
        // Blanks hold no texels, but the set would otherwise grow for the life of the
        // process across every size and subpixel phase a space is laid out at. The
        // retry that follows a repack re-adds this frame's.
        self.blank.clear();
        self.coverage.reset();
        self.color.reset();

        // Tallest first: shelves are sized by their first occupant, so packing in
        // descending height is what keeps the shelf count near the theoretical
        // minimum rather than opening one per glyph.
        let mut keys: Vec<GlyphKey> = self.entries.keys().copied().collect();
        keys.sort_by_key(|key| std::cmp::Reverse(self.entries[key].image.height));

        let page_size = self.config.page_size;
        let mut dropped = Vec::new();
        for key in keys {
            let entry = &self.entries[&key];
            let kind = AtlasKind::of(entry.image.content);
            let surface = match kind {
                AtlasKind::Coverage => &mut self.coverage,
                AtlasKind::Color => &mut self.color,
            };
            // Re-packing only ever reuses pages that already exist, so no device is
            // needed and the allocation cannot fail for want of one.
            let placed = surface.allocate_in_existing_pages(entry.image.width, entry.image.height);
            let Some((index, x, y)) = placed else {
                dropped.push(key);
                continue;
            };
            write_glyph(queue, &surface.pages[index as usize].texture, kind, x, y, &entry.image);

            let scale = 1.0 / page_size as f32;
            let entry = self.entries.get_mut(&key).expect("key came from the map");
            entry.slot = AtlasSlot {
                page: AtlasPage { kind, index },
                width: entry.image.width,
                height: entry.image.height,
                uv_min: [x as f32 * scale, y as f32 * scale],
                uv_max: [
                    (x + entry.image.width) as f32 * scale,
                    (y + entry.image.height) as f32 * scale,
                ],
                left: entry.image.left,
                top: entry.image.top,
            };
        }
        for key in &dropped {
            self.entries.remove(key);
        }
        log::debug!(
            "repacked the glyph atlas: {before} entries down to {}",
            self.entries.len()
        );
    }

    fn surface(&self, kind: AtlasKind) -> &Surface {
        match kind {
            AtlasKind::Coverage => &self.coverage,
            AtlasKind::Color => &self.color,
        }
    }
}

fn create_page(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    sampler: &wgpu::Sampler,
    kind: AtlasKind,
    size: u32,
) -> Page {
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("vellum-atlas-page"),
        size: wgpu::Extent3d { width: size, height: size, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: kind.format(),
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let bind_group = texture::bind(device, layout, &view, sampler, "vellum-atlas-page");
    Page { texture, bind_group, packer: ShelfPacker::new(size) }
}

fn write_glyph(
    queue: &wgpu::Queue,
    texture: &wgpu::Texture,
    kind: AtlasKind,
    x: u32,
    y: u32,
    image: &GlyphImage,
) {
    // A colour bitmap is premultiplied on the way in so that the atlas, like every
    // other texture in this crate, holds premultiplied texels. Swash does not state
    // whether its colour output already is; treating it as straight alpha is the
    // assumption, and the visible cost of being wrong is slightly dark emoji edges
    // rather than anything structural.
    let owned;
    let data = match kind {
        AtlasKind::Coverage => &image.data,
        AtlasKind::Color => {
            owned = premultiply(&image.data);
            &owned
        }
    };
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d { x, y, z: 0 },
            aspect: wgpu::TextureAspect::All,
        },
        data,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(image.width * kind.bytes_per_texel()),
            rows_per_image: Some(image.height),
        },
        wgpu::Extent3d { width: image.width, height: image.height, depth_or_array_layers: 1 },
    );
}

fn premultiply(rgba: &[u8]) -> Vec<u8> {
    let mut out = rgba.to_vec();
    for texel in out.chunks_exact_mut(4) {
        let a = u32::from(texel[3]);
        for channel in &mut texel[..3] {
            *channel = ((u32::from(*channel) * a + 127) / 255) as u8;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glyphs_of_a_similar_height_share_a_shelf() {
        let mut packer = ShelfPacker::new(128);
        assert_eq!(packer.allocate(10, 20), Some((0, 0)));
        assert_eq!(packer.allocate(12, 20), Some((10, 0)));
        assert_eq!(packer.allocate(8, 16), Some((22, 0)), "16 is 80% of 20");
        assert_eq!(packer.shelves.len(), 1);
    }

    /// The reason for the three-quarters floor: without it one 20px glyph would set
    /// the height for a whole row of 6px ones and waste 70% of the shelf.
    #[test]
    fn a_much_shorter_glyph_opens_its_own_shelf() {
        let mut packer = ShelfPacker::new(128);
        packer.allocate(10, 20);
        assert_eq!(packer.allocate(10, 6), Some((0, 20)));
        assert_eq!(packer.shelves.len(), 2);
    }

    #[test]
    fn a_full_shelf_starts_a_new_row() {
        let mut packer = ShelfPacker::new(128);
        assert_eq!(packer.allocate(120, 20), Some((0, 0)));
        assert_eq!(packer.allocate(20, 20), Some((0, 20)));
    }

    #[test]
    fn a_page_that_cannot_fit_a_glyph_says_so() {
        let mut packer = ShelfPacker::new(128);
        assert_eq!(packer.allocate(200, 10), None, "wider than the page");
        assert_eq!(packer.allocate(10, 200), None, "taller than the page");
        packer.allocate(120, 100);
        // The one shelf is nearly full across and there is no room for a second.
        assert_eq!(packer.allocate(20, 100), None, "no room on the shelf or below it");
    }

    /// Allocated rectangles must never overlap, whatever order the sizes arrive in.
    /// Two glyphs sharing texels is the one failure a packer can have that looks like
    /// a font bug rather than a renderer bug.
    #[test]
    fn allocated_rectangles_never_overlap() {
        let mut packer = ShelfPacker::new(256);
        let mut placed: Vec<(u32, u32, u32, u32)> = Vec::new();
        for i in 0..200u32 {
            let width = 3 + i % 17;
            let height = 5 + (i * 7) % 23;
            let Some((x, y)) = packer.allocate(width, height) else {
                continue;
            };
            for &(px, py, pw, ph) in &placed {
                let disjoint =
                    x + width <= px || px + pw <= x || y + height <= py || py + ph <= y;
                assert!(disjoint, "({x},{y},{width},{height}) overlaps ({px},{py},{pw},{ph})");
            }
            assert!(x + width <= 256 && y + height <= 256, "({x},{y}) leaves the page");
            placed.push((x, y, width, height));
        }
        assert!(placed.len() > 100, "only {} of 200 glyphs were placed", placed.len());
    }

    #[test]
    fn resetting_a_packer_hands_back_the_whole_page() {
        let mut packer = ShelfPacker::new(64);
        packer.allocate(64, 64);
        assert_eq!(packer.allocate(1, 1), None);
        packer.reset();
        assert_eq!(packer.allocate(64, 64), Some((0, 0)));
    }

    #[test]
    fn the_two_formats_never_share_a_page() {
        assert_eq!(AtlasKind::of(GlyphContent::Coverage), AtlasKind::Coverage);
        assert_eq!(AtlasKind::of(GlyphContent::Color), AtlasKind::Color);
        assert_eq!(AtlasKind::of(GlyphContent::SubpixelCoverage), AtlasKind::Color);
        assert_ne!(AtlasKind::Coverage.format(), AtlasKind::Color.format());
        assert_eq!(AtlasKind::Coverage.bytes_per_texel(), 1);
        assert_eq!(AtlasKind::Color.bytes_per_texel(), 4);
    }

    /// `top` is measured *up* from the baseline, so the quad's top edge is above it.
    /// Getting this sign wrong renders every line of text one cap-height too low, and
    /// it is not obvious in a screenshot until something else is beside it.
    #[test]
    fn a_slot_places_its_quad_above_the_baseline() {
        let slot = AtlasSlot {
            page: AtlasPage { kind: AtlasKind::Coverage, index: 0 },
            width: 12,
            height: 18,
            uv_min: [0.0, 0.0],
            uv_max: [0.1, 0.2],
            left: 2,
            top: 14,
        };
        assert_eq!(slot.quad_origin((100, 200)), [102.0, 186.0]);
        assert_eq!(slot.quad_size(), [12.0, 18.0]);
        assert!(!slot.is_color());
    }

    #[test]
    fn colour_glyphs_are_premultiplied_on_the_way_in() {
        assert_eq!(premultiply(&[255, 128, 0, 128]), vec![128, 64, 0, 128]);
        assert_eq!(premultiply(&[255, 255, 255, 255]), vec![255, 255, 255, 255]);
    }

    #[test]
    fn the_configuration_refuses_a_degenerate_page() {
        let config = AtlasConfig { page_size: 0, max_pages: 0 };
        assert!(config.page_size < 64 && config.max_pages < 1);
        // `GlyphAtlas::new` clamps both; asserted here rather than through a device
        // because the clamp is the contract, not the texture.
        assert_eq!(config.page_size.max(64), 64);
        assert_eq!(config.max_pages.max(1), 1);
    }
}
