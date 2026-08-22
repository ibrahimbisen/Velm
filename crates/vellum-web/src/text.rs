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
    /// What an auto-fitted block resolved to, keyed on the item and the box it fits.
    ///
    /// Cached separately from the layout because it is *much* more expensive: a fit is a
    /// binary search that shapes the text about fourteen times, and it has to happen before
    /// the layout key can even be computed. Uncached it would run every frame, on every
    /// sticky on screen, for the life of the tab.
    fitted: HashMap<(SceneId, u64, u32, u32), f32>,
    /// Reused every frame. The glyph list is rebuilt per frame but its allocation is not.
    glyphs: Vec<(GlyphKey, GlyphImage)>,
    /// Blocks too small to shape, drawn as bars on the type's rhythm.
    greeked: Vec<Greek>,
    /// What was queued this frame, in the order it should draw.
    ///
    /// One list rather than the caller holding a parallel one: the key a block was shaped
    /// under and the key it is drawn under have to be the same value, and the surest way to
    /// guarantee that is for only one place to compute it.
    pending: Vec<(Key, [f32; 2], f32, Rgba)>,
}

/// Below this many device pixels, text is greeked rather than shaped.
///
/// At a fitted camera a 14-unit label is a fraction of a pixel tall: rasterising it produces
/// noise, and rasterising a thousand of them produces noise slowly. But drawing *nothing*
/// there is worse than either — a board of stickies becomes a board of blank white boxes,
/// which is what it looked like beside the desktop app, and the desktop app has greeked at
/// this size since it was written.
const MIN_DEVICE_FONT_SIZE: f32 = 5.0;

/// Below this, not even a bar.
///
/// A greeked line is a quad about a pixel tall. Under it there is nothing left to say — a
/// sub-pixel bar is a faint smear, and a thousand of them is a grey wash over the board that
/// hides the shapes underneath rather than suggesting the words on top of them.
const MIN_GREEK_PIXELS: f32 = 1.0;

/// How tall a greeked bar is against the line it stands for, and how much of the line's
/// width the last one takes.
///
/// Not arbitrary: a bar as tall as its line is a solid block, and a last line as long as the
/// others reads as a rectangle rather than as a paragraph. Both numbers are what make a
/// stack of quads read as writing at a glance.
const GREEK_WEIGHT: f32 = 0.42;
const GREEK_LAST_LINE: f32 = 0.62;

/// One block that was too small to shape, as the bars that stand in for it.
///
/// Everything is already in **screen** pixels: the caller has the zoom and this is drawn in
/// the screen view beside the glyphs, so converting once here beats carrying the zoom
/// through to the drawing pass and converting there.
struct Greek {
    origin: [f32; 2],
    size: [f32; 2],
    line_height: f32,
    characters: usize,
    /// What one character costs, on average, at this size.
    advance: f32,
    color: Rgba,
}

/// Line height as a multiple of the font size, and average advance as a fraction of it.
///
/// ⚠ **Estimates, deliberately, and that is the whole point of greeking.** Asking the shaper
/// for the real numbers means shaping — which is exactly the work being avoided, on exactly
/// the blocks it is least worth doing. `vellum-app`'s `GreekLines::Estimated` makes the same
/// trade and its comment says so. The advance is measured for Inter at a mixed-case average.
const LINE_HEIGHT: f32 = 1.25;
const AVERAGE_ADVANCE: f32 = 0.5;

impl TextLayer {
    pub fn new() -> Result<Self, String> {
        // `with_fonts` deliberately, not `new`: `new` scans the system's fonts, which is a
        // no-op on wasm but does real work natively, and this crate only ever runs on wasm.
        let engine = TextEngine::with_fonts(BUNDLED_FONTS.iter().map(|f| f.to_vec()))
            .map_err(|e| format!("cannot start the text engine: {e}"))?;
        Ok(Self {
            engine,
            layouts: HashMap::new(),
            fitted: HashMap::new(),
            glyphs: Vec::new(),
            greeked: Vec::new(),
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
        self.fitted.retain(|(item, ..), _| live.contains(item));
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
        // The style's own size, or `None` for auto-fit.
        //
        // ⚠ `None` is not "use the default" — it is Miro's own convention, arriving through
        // `Style::font_size`, and **every sticky on the reference board uses it**. Treating
        // it as a 14-unit default is why a sticky whose words fill a 400-unit box was drawn
        // at a size that vanished at any fitted zoom, which is what made the browser's board
        // a field of blank white rectangles beside the desktop app's.
        font_size: Option<f32>,
        zoom: f32,
        color: Rgba,
    ) -> bool {
        if text.is_empty() {
            return false;
        }
        let font_size = self.resolve_size(item, generation, text, size, font_size);
        // The test is in *device* pixels — that is what "too small to read" means — even
        // though everything shaped below is in world units.
        if font_size * zoom < MIN_DEVICE_FONT_SIZE {
            self.greeked.push(Greek {
                origin,
                size: [size[0] * zoom, size[1] * zoom],
                line_height: font_size * LINE_HEIGHT * zoom,
                characters: text.char_len(),
                advance: font_size * AVERAGE_ADVANCE * zoom,
                color,
            });
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

    /// The size a block is set at: the style's own, or the largest that fits its box.
    fn resolve_size(
        &mut self,
        item: SceneId,
        generation: u64,
        text: &StyledText,
        size: [f32; 2],
        style_size: Option<f32>,
    ) -> f32 {
        if let Some(size) = style_size
            && size.is_finite()
            && size > 0.0
        {
            return size;
        }
        let key = (item, generation, (size[0] * 10.0) as u32, (size[1] * 10.0) as u32);
        if let Some(fitted) = self.fitted.get(&key) {
            return *fitted;
        }
        let area = vellum_text::FitBox::new(size[0].max(1.0), size[1].max(1.0));
        let params = LayoutParams { max_width: Some(area.width), ..Default::default() };
        let resolved =
            self.engine.fit_font_size(text, &params, area, &vellum_text::AutoFit::default());
        self.fitted.insert(key, resolved);
        resolved
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

    /// Draw the blocks that were too small to shape, as bars on the type's rhythm.
    ///
    /// Called whether or not anything was shaped — a fitted board is *entirely* greeked, so
    /// folding this into [`Self::flush`]'s early return would make the one case it exists
    /// for the one case it never runs in. That is the shape of bug this repository keeps
    /// finding, so it is its own call.
    pub fn flush_greeked(&mut self, list: &mut DrawList) -> usize {
        let mut drawn = 0;
        for block in self.greeked.drain(..) {
            let height = (block.line_height * GREEK_WEIGHT).max(0.0);
            if height < MIN_GREEK_PIXELS || block.line_height <= 0.0 || block.advance <= 0.0 {
                continue;
            }
            let per_line = (block.size[0] / block.advance).floor().max(1.0);
            let wanted = (block.characters as f32 / per_line).ceil().max(1.0);
            // Never more lines than the box has room for. A sticky whose words overflow it
            // draws a full box of bars rather than bars running out of the bottom, which is
            // both what the shaped path does and what the box actually looks like.
            let fits = (block.size[1] / block.line_height).floor().max(1.0);
            let lines = wanted.min(fits) as usize;

            // Greeked text is lighter than set text: the bars stand for words, and at full
            // strength a paragraph of them is a black slab where the real thing is grey.
            let ink = block.color.with_alpha(block.color.a * 0.55);
            for line in 0..lines {
                let y = block.origin[1] + line as f32 * block.line_height;
                let last = line + 1 == lines && lines > 1;
                let width = if last { block.size[0] * GREEK_LAST_LINE } else { block.size[0] };
                list.push_quad(vellum_render::QuadInstance::solid(
                    [block.origin[0], y],
                    [width.max(1.0), height],
                    ink,
                ));
                drawn += 1;
            }
        }
        drawn
    }
}

use vellum_text::BUNDLED_FONTS;
