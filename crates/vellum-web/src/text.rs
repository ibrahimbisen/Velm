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
//! edit to blame it on.
//!
//! ⚠ **The key is `(item, slot)`, and the generation, the size and the wrap width are all
//! *fields* on the value.** They were key components, and this line said so — which is the
//! `locked: false` trap in the file that has been re-keyed twice: whatever a block was shaped
//! against, it must **replace** its entry rather than mint a new one. In the key, a synced
//! edit that refits a sticky to a different size strands the old layout for as long as the
//! item stays visible, in wasm memory that never returns to the OS. `shapes.rs` and
//! `strokes.rs` had the right shape all along — `SceneId` alone, everything else in the
//! value — and this was the only one that accumulated.
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
    /// Which of this item's labels. `0` for everything with one block — a sticky, a frame's
    /// name, a card.
    ///
    /// ⚠ **Without this a table's cells share one cache entry.** The key was written when an
    /// item had exactly one block, and the rest of it — the generation, the size, the wrap
    /// width — is identical for every cell in a column, so two cells with the same geometry
    /// would resolve to the same `Layout` and the second would draw the first one's words.
    /// `draw.rs` has carried the same field as `BlockKey::new(id, slot)` since tables were
    /// written, and states the sharper half of the hazard: a *stale* slot draws the caret on
    /// one card while typing into another.
    slot: u16,
}

/// A shaped block, and which version of its item it was shaped from.
///
/// ⚠ **The generation is a *field*, not part of the key, and that distinction is the whole
/// difference between a cache and a leak.** In the key, a changed item mints a new entry and
/// leaves the old one behind — and nothing removes it while the item stays on screen, because
/// `retain_visible` prunes by item. That was harmless while a board could not change: the
/// stamp never moved. `crate::live` is what arms it — every merge calls
/// `Projection::rebuild`, which restamps every item that changed — so a tab left open on a
/// board somebody is editing accumulated one `Layout` per edit per visible item, for ever, in
/// a linear memory that never returns to the OS.
///
/// As a field it *replaces*, which is what `vellum-app`'s `BlockKey` does and what this
/// module's own siblings `shapes.rs` and `strokes.rs` already did. Only this one accumulated.
struct Shaped {
    generation: u64,
    /// Font size in tenths of a **world** unit, and the wrap width in the same units.
    ///
    /// World rather than device, so neither moves with the camera. Held against a *device*
    /// size, a layout would be re-shaped on every frame of a zoom — which is both the cost
    /// this cache exists to avoid and, worse, a visible re-wrap.
    ///
    /// ⚠ **Fields, not key components, for the same reason `generation` is.** They were in
    /// the key, and the first fix moved only the generation out — an N−1 fix, and the case it
    /// left behind is the common one: **a sticky is auto-fitted** (`font_size: None`, which
    /// is every sticky on the reference board), so a merge that changes its words refits to a
    /// *different size*, mints a different key, and strands the old entry. A synced resize
    /// strands one `Layout` per tenth-of-a-unit step, in memory that never returns to the OS.
    ///
    /// With them here the entry is replaced, which is what `shapes.rs` and `strokes.rs` do —
    /// both key on `SceneId` alone and carry everything else in the value.
    size_tenths: u32,
    width_tenths: u32,
    layout: Layout,
}

pub struct TextLayer {
    engine: TextEngine,
    layouts: HashMap<Key, Shaped>,
    /// What an auto-fitted block resolved to, keyed on the item and the box it fits.
    ///
    /// Cached separately from the layout because it is *much* more expensive: a fit is a
    /// binary search that shapes the text about fourteen times, and it has to happen before
    /// the layout key can even be computed. Uncached it would run every frame, on every
    /// sticky on screen, for the life of the tab.
    /// Keyed without the generation, for [`Shaped`]'s reason; the stamp rides in the value.
    fitted: HashMap<(SceneId, u16), (u64, u32, u32, f32)>,
    /// Reused every frame. The glyph list is rebuilt per frame but its allocation is not.
    glyphs: Vec<(GlyphKey, GlyphImage)>,
    /// Blocks too small to shape, drawn as bars on the type's rhythm.
    greeked: Vec<Greek>,
    /// The zoom the glyph bitmaps in the engine were rasterised for.
    ///
    /// ⚠ **A `GlyphKey` carries the device size and the subpixel phase**, so every distinct
    /// zoom mints a whole new set of them, and `TextEngine`'s bitmap cache has no eviction of
    /// its own — its own doc says so and names `forget_glyph_bitmaps` as the release valve.
    /// Without this field the valve is never pulled: a pinch produces a fresh zoom on every
    /// frame of the gesture, and on wasm linear memory never returns to the OS, so the peak
    /// of one pinch becomes the tab's footprint for the rest of its life. `draw.rs` keeps
    /// exactly this field for exactly this reason.
    last_scale: f32,
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
    anchor: crate::layout::Anchor,
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
            last_scale: 0.0,
            pending: Vec::new(),
        })
    }

    /// The shaper itself, for a layer that has to *measure* before it can lay anything out.
    ///
    /// ⚠ Handed out rather than duplicated, and that is the point. A mind map's geometry comes
    /// from its shaped labels — the tree's extent is determined by how wide the words are — and
    /// a table's row heights come from the same place. A second `TextEngine` would be a second
    /// font database and a second glyph cache on a device that has neither to spare, and worse,
    /// two shapers that could disagree about the width of the same string: the browser would
    /// then lay a table out to one measurement and draw its words at another.
    pub fn engine_mut(&mut self) -> &mut TextEngine {
        &mut self.engine
    }

    /// Drop everything shaped for items that are no longer on screen.
    ///
    /// Without this the cache is a leak with a slow fuse: pan across a large board and every
    /// block ever visible stays shaped, holding its glyph bitmaps, for the life of the tab —
    /// and wasm linear memory never returns to the OS, so the peak becomes permanent.
    /// ⚠ No early return on a size. It used to skip while `layouts.len() < 512`, and
    /// `fitted` was gated on the same number — but a greeked block never inserts into
    /// `layouts`, so on a fitted board `layouts` stays at **zero**, the guard fires every
    /// frame, and `fitted` grows by one per auto-fitted item ever seen and is never released.
    /// On a board with fewer than 512 text items neither map ever pruned at all.
    pub fn retain_visible(&mut self, visible: &[SceneId]) {
        if self.layouts.is_empty() && self.fitted.is_empty() {
            return;
        }
        let live: std::collections::HashSet<SceneId> = visible.iter().copied().collect();
        self.layouts.retain(|key, _| live.contains(&key.item));
        self.fitted.retain(|(item, _), _| live.contains(item));
    }

    /// Shape and queue one item's text. Returns `false` if it was too small to draw.
    #[allow(clippy::too_many_arguments)]
    pub fn queue(
        &mut self,
        item: SceneId,
        // Which of this item's labels — see `Key::slot`. `0` for an item with one block.
        slot: u16,
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
        anchor: crate::layout::Anchor,
    ) -> bool {
        if text.is_empty() {
            return false;
        }
        // ⚠ The greek test comes **before** the fit, on an upper bound taken from the box,
        // and the order is the whole point: resolving an auto-fitted size is a binary search
        // that shapes the text about eleven times, and greeking exists precisely to avoid
        // shaping. Resolving first made a fitted board pay one of those searches per sticky
        // on its first frame and throw every answer away. `draw.rs` greeks on the same bound
        // and states the invariant: a greeked block is "sized from the type, not from a
        // layout". The bound is exact rather than a guess — auto-fit can never return a size
        // whose line height does not fit the box.
        //
        // The test is in *device* pixels — that is what "too small to read" means — even
        // though everything shaped below is in world units.
        let ceiling = font_size.unwrap_or((size[1] / LINE_HEIGHT).max(1.0));
        if ceiling * zoom < MIN_DEVICE_FONT_SIZE {
            self.greeked.push(Greek {
                origin,
                size: [size[0] * zoom, size[1] * zoom],
                line_height: ceiling * LINE_HEIGHT * zoom,
                characters: text.char_len(),
                advance: ceiling * AVERAGE_ADVANCE * zoom,
                color,
                anchor,
            });
            return false;
        }
        let font_size = self.resolve_size(item, slot, generation, text, size, font_size);
        let key = Key { item, slot };
        let (size_tenths, width_tenths) = ((font_size * 10.0) as u32, (size[0] * 10.0) as u32);
        // Reshaped when anything it was shaped against has moved, and **replaced rather than
        // added** — one entry per block, for the life of that block's visibility.
        let stale = self.layouts.get(&key).is_none_or(|held| {
            held.generation != generation
                || held.size_tenths != size_tenths
                || held.width_tenths != width_tenths
        });
        if stale {
            let layout = self.engine.layout(
                text,
                &LayoutParams {
                    font_size,
                    max_width: Some(size[0].max(1.0)),
                    ..Default::default()
                },
            );
            self.layouts.insert(key, Shaped { generation, size_tenths, width_tenths, layout });
        }
        let layout = &self.layouts[&key].layout;
        // Rasterised at the size it will actually be drawn, which is what keeps a zoomed-in
        // glyph sharp rather than a magnified small one.
        // ⚠ The anchor is applied **after** shaping, because centring needs the laid-out
        // extent and the extent is what shaping produces. Guessing it from the box — which is
        // the version that does not need a second pass — puts a one-line sticky's words in
        // the middle of a box sized for four.
        let placed = match anchor {
            crate::layout::Anchor::TopLeft => origin,
            crate::layout::Anchor::Centred => [
                origin[0] + (size[0] - layout.extent.width).max(0.0) * 0.5 * zoom,
                origin[1] + (size[1] - layout.extent.height).max(0.0) * 0.5 * zoom,
            ],
        };
        // Rasterisation is deferred to `flush`, which is the only place that holds the
        // atlas — and holding the atlas is what makes it possible to rasterise *only* what
        // the atlas is short of. See there.
        self.pending.push((key, placed, zoom, color));
        true
    }

    /// The size a block is set at: the style's own, or the largest that fits its box.
    fn resolve_size(
        &mut self,
        item: SceneId,
        slot: u16,
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
        let key = (item, slot);
        let (w, h) = ((size[0] * 10.0) as u32, (size[1] * 10.0) as u32);
        if let Some((stamp, box_w, box_h, fitted)) = self.fitted.get(&key)
            && *stamp == generation
            && *box_w == w
            && *box_h == h
        {
            return *fitted;
        }
        let area = vellum_text::FitBox::new(size[0].max(1.0), size[1].max(1.0));
        let params = LayoutParams { max_width: Some(area.width), ..Default::default() };
        let resolved =
            self.engine.fit_font_size(text, &params, area, &vellum_text::AutoFit::default());
        self.fitted.insert(key, (generation, w, h, resolved));
        resolved
    }

    /// Upload this frame's glyphs, then draw every queued block.
    ///
    /// Upload and draw are one step because they must not be able to disagree: a block drawn
    /// against an atlas that does not hold its glyphs draws nothing, silently, and
    /// `push_layout`'s only complaint is a count of missing slots that nobody reads.
    /// Drop every rasterised glyph if the zoom has moved since the last frame.
    ///
    /// Called once per frame, before anything is queued. Cheap when the zoom held — one
    /// float compare — and the only thing that bounds the bitmap cache when it did not.
    pub fn note_scale(&mut self, zoom: f32) {
        if zoom != self.last_scale {
            self.engine.forget_glyph_bitmaps();
            self.last_scale = zoom;
        }
    }

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
        // ⚠ **Rasterise only what the atlas is actually short of**, which after the first
        // frame at a given zoom is never. `atlas_entries` allocates a map and a vector per
        // block and clones every glyph's bitmap out of the engine's cache, so asking for
        // every visible block every frame is an allocation and a memcpy per glyph, sixty
        // times a second, handed to an atlas that already holds the keys and skips them.
        // `draw.rs` probes first and says why in the same words.
        self.glyphs.clear();
        let pending = std::mem::take(&mut self.pending);
        for (key, origin, zoom, _) in &pending {
            let Some(layout) = self.layouts.get(key).map(|held| &held.layout) else { continue };
            let short = layout.glyphs().any(|glyph| {
                let physical = glyph.physical((origin[0], origin[1]), *zoom).key;
                atlas.slot(physical).is_none() && !atlas.is_blank(physical)
            });
            if short {
                self.glyphs
                    .extend(self.engine.atlas_entries(layout, (origin[0], origin[1]), *zoom));
            }
        }
        if atlas.prepare(device, queue, &self.glyphs).is_err() {
            // A full atlas is not fatal: the glyphs that did fit still draw. Reporting it
            // matters more than recovering from it, because the cause is a configuration
            // problem rather than a transient one.
            log::warn!("glyph atlas full — some text will not draw this frame");
        }
        self.glyphs.clear();

        let mut drawn = 0;
        for (key, origin, zoom, color) in pending {
            if let Some(layout) = self.layouts.get(&key).map(|held| &held.layout) {
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
            // Centred blocks stack their bars from the middle, like the words they stand in
            // for — a stack pinned to the top of a sticky reads as a different layout, which
            // makes the board change shape at the zoom where greeking starts.
            let stack = lines as f32 * block.line_height;
            let top = match block.anchor {
                crate::layout::Anchor::TopLeft => block.origin[1],
                crate::layout::Anchor::Centred => {
                    block.origin[1] + (block.size[1] - stack).max(0.0) * 0.5
                }
            };
            for line in 0..lines {
                let y = top + line as f32 * block.line_height;
                let last = line + 1 == lines && lines > 1;
                let width = if last { block.size[0] * GREEK_LAST_LINE } else { block.size[0] };
                let x = match block.anchor {
                    crate::layout::Anchor::TopLeft => block.origin[0],
                    crate::layout::Anchor::Centred => {
                        block.origin[0] + (block.size[0] - width).max(0.0) * 0.5
                    }
                };
                list.push_quad(vellum_render::QuadInstance::solid(
                    [x, y],
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
