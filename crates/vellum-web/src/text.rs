//! Shaping a board's words and getting the glyphs onto the GPU.
//!
//! Three steps, and none of them are new — this is `vellum-app`'s pipeline with the parts a
//! viewer does not need left out:
//!
//! 1. [`vellum_text::TextEngine::layout`] shapes an item's [`StyledText`] into a [`Layout`].
//! 2. `atlas_entries` rasterises the glyphs that layout needs, at the size it will be drawn.
//! 3. [`vellum_render::GlyphAtlas::prepare`] uploads them, and `push_layout` draws them.
//!
//! # Why the cache is keyed on the item's own generation
//!
//! `vellum-app` learned this the expensive way and the comment on `Projected::generation`
//! records it: caching on the *projection's* stamp means any edit anywhere re-shapes **every
//! visible block**, sixty times a second, through cosmic-text. Here nothing edits at all, so
//! a wrong key would simply reshape every block every frame forever — the same cost with no
//! edit to blame it on. The key is `(item, generation, quantised size)`.
//!
//! # Fonts
//!
//! Inter only, from [`vellum_text::BUNDLED_FONTS`], which are `include_bytes!`'d into the
//! binary. That is not a limitation of the port so much as the honest state of the web:
//! `fontdb`'s system scan compiles out to a no-op on wasm, so a board asking for a named
//! family it does not carry gets Inter. Native has the same gap for any family the machine
//! lacks (trap 10); the browser just has it for *every* family.

use std::collections::HashMap;

use vellum_render::{DrawList, GlyphAtlas, Rgba};
use vellum_scene::ItemId as SceneId;
use vellum_text::{GlyphImage, GlyphKey, Layout, LayoutParams, StyledText, TextEngine};

/// What a cached layout was shaped for. Reshaped when any of it moves.
#[derive(PartialEq, Eq, Hash, Clone, Copy)]
struct Key {
    item: SceneId,
    generation: u64,
    /// Font size in tenths of a **world** unit.
    ///
    /// World rather than device, so the key does not move with the camera. Keyed on a
    /// device size, a layout would be re-shaped on every frame of a zoom -- which is both
    /// the cost the cache exists to avoid and, worse, a visible re-wrap.
    size_tenths: u32,
    /// Wrap width, in world units for the same reason.
    width_tenths: u32,
}

pub struct TextLayer {
    engine: TextEngine,
    layouts: HashMap<Key, Layout>,
    /// Reused every frame. The glyph list is rebuilt per frame but its allocation is not.
    glyphs: Vec<(GlyphKey, GlyphImage)>,
    /// What was queued this frame, in the order it should draw.
    ///
    /// One list rather than the caller holding a parallel one: the key a block was shaped
    /// under and the key it is drawn under have to be the same value, and the surest way to
    /// guarantee that is for only one place to compute it.
    pending: Vec<(Key, [f32; 2], f32, Rgba)>,
}

/// Below this many device pixels, text is not drawn at all.
///
/// `vellum-app` greeks instead — grey bars on the type's rhythm — which is the better answer
/// and needs the painter's block machinery. Here the choice is draw or skip, and skipping is
/// right: at a fitted camera a 14px label is a fraction of a pixel tall, so rasterising it
/// produces noise, and rasterising a thousand of them produces noise slowly.
const MIN_DEVICE_FONT_SIZE: f32 = 5.0;

impl TextLayer {
    pub fn new() -> Result<Self, String> {
        // `with_fonts` deliberately, not `new`: `new` scans the system's fonts, which is a
        // no-op on wasm but does real work natively, and this crate only ever runs on wasm.
        let engine = TextEngine::with_fonts(BUNDLED_FONTS.iter().map(|f| f.to_vec()))
            .map_err(|e| format!("cannot start the text engine: {e}"))?;
        Ok(Self {
            engine,
            layouts: HashMap::new(),
            glyphs: Vec::new(),
            pending: Vec::new(),
        })
    }

    /// Drop everything shaped for items that are no longer on screen.
    ///
    /// Without this the cache is a leak with a slow fuse: pan across a large board and every
    /// block ever visible stays shaped, holding its glyph bitmaps, for the life of the tab —
    /// and wasm linear memory never returns to the OS, so the peak becomes permanent.
    pub fn retain_visible(&mut self, visible: &[SceneId]) {
        if self.layouts.len() < 512 {
            return;
        }
        let live: std::collections::HashSet<SceneId> = visible.iter().copied().collect();
        self.layouts.retain(|key, _| live.contains(&key.item));
    }

    /// Shape and queue one item's text. Returns `false` if it was too small to draw.
    #[allow(clippy::too_many_arguments)]
    pub fn queue(
        &mut self,
        item: SceneId,
        generation: u64,
        text: &StyledText,
        // Top-left of the text box, in the same camera-relative pixels the quads use.
        origin: [f32; 2],
        // Box size in **world** units. Shaping is zoom-independent; only the draw scales.
        size: [f32; 2],
        font_size: f32,
        zoom: f32,
        color: Rgba,
    ) -> bool {
        // The skip test is in *device* pixels -- that is what "too small to read" means --
        // even though everything shaped below is in world units.
        if text.is_empty() || font_size * zoom < MIN_DEVICE_FONT_SIZE {
            return false;
        }
        let key = Key {
            item,
            generation,
            size_tenths: (font_size * 10.0) as u32,
            width_tenths: (size[0] * 10.0) as u32,
        };
        let layout = self.layouts.entry(key).or_insert_with(|| {
            self.engine.layout(
                text,
                &LayoutParams {
                    font_size,
                    max_width: Some(size[0].max(1.0)),
                    ..Default::default()
                },
            )
        });
        // Rasterised at the size it will actually be drawn, which is what keeps a zoomed-in
        // glyph sharp rather than a magnified small one.
        self.glyphs
            .extend(self.engine.atlas_entries(layout, (origin[0], origin[1]), zoom));
        self.pending.push((key, origin, zoom, color));
        true
    }

    /// Upload this frame's glyphs, then draw every queued block.
    ///
    /// Upload and draw are one step because they must not be able to disagree: a block drawn
    /// against an atlas that does not hold its glyphs draws nothing, silently, and
    /// `push_layout`'s only complaint is a count of missing slots that nobody reads.
    pub fn flush(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        atlas: &mut GlyphAtlas,
        list: &mut DrawList,
    ) -> usize {
        if self.pending.is_empty() {
            return 0;
        }
        if atlas.prepare(device, queue, &self.glyphs).is_err() {
            // A full atlas is not fatal: the glyphs that did fit still draw. Reporting it
            // matters more than recovering from it, because the cause is a configuration
            // problem rather than a transient one.
            log::warn!("glyph atlas full — some text will not draw this frame");
        }
        self.glyphs.clear();

        let mut drawn = 0;
        for (key, origin, zoom, color) in self.pending.drain(..) {
            if let Some(layout) = self.layouts.get(&key) {
                list.push_layout(atlas, layout, origin, zoom, color);
                drawn += 1;
            }
        }
        drawn
    }
}

use vellum_text::BUNDLED_FONTS;
