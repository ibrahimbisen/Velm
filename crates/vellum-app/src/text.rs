//! Laying out an item's text once, and keeping it.
//!
//! Shaping is the most expensive thing a frame can do — cosmic-text has to run the
//! bidi algorithm, pick faces, shape runs and wrap them — and it produces a result
//! that only changes when the *text* does. It emphatically does not change when the
//! camera moves: [`vellum_text::Layout`] is in logical pixels, block-relative, and
//! the zoom is applied when a glyph is turned into a pixel-grid position. So a
//! layout is computed on edit and reused on every frame after it, which is what
//! makes panning a board of 429 text strings free.
//!
//! Miro's auto-fit (`fs: 0, fsa: 1`) is *every sticky on the reference board*, and
//! it binary-searches a dozen layouts to find the size. That is emphatically a
//! once-per-edit cost, and it is the reason this cache exists rather than a comment
//! saying shaping is cheap.
//!
//! # The one thing that is per-frame
//!
//! Rasterisation. A glyph bitmap is scale-dependent by construction — that is what
//! [`vellum_text::GlyphKey`] carries a device size for — so the atlas has to be
//! asked for the current zoom every frame. [`TextCache::layout`] does not do that;
//! `draw` does, and it only rasterises the keys the atlas says it is missing.

use std::collections::HashMap;

use vellum_doc::{Align, Style, StyledText as DocText};
use vellum_text::{
    AutoFit, FitBox, Layout, LayoutParams, Rgb, SpanStyle, StyledText, TextAlign, TextEngine,
    TextSpan,
};

use vellum_scene::ItemId as SceneId;

/// Font size below which text is not drawn at all, in **device** pixels.
///
/// At the zoom that fits the reference board's 41282 px width into a laptop window,
/// a 14 px sticky label is a third of a pixel tall: rasterising it produces noise,
/// and rasterising 429 of them produces noise slowly. Miro drops text at small
/// zooms for the same reason. Five pixels is where a lowercase letter stops having
/// an interior.
pub const MIN_DEVICE_FONT_SIZE: f32 = 5.0;

/// Miro's default body family. Named explicitly rather than left to the platform
/// default because every widget on an imported board asks for it by name in `ffn`,
/// and falling back to the system sans changes every wrap point on the board.
/// The family every item that has not named one is set in.
///
/// **The bundled one** — see `vellum_text::BUNDLED_FONTS`. It was `"Noto Sans"`, and that is
/// not a harmless stale constant: `params_for` puts this into `LayoutParams::font_family`
/// *explicitly*, so every default-family item on the canvas asked for Noto Sans by name and
/// the `sans-serif` alias the bundle points at was never consulted. Bundling Inter and giving
/// the app a real Bold therefore changed nothing on the board — `family_has_bold` was being
/// asked about Noto Sans, which ships regular only, and correctly dropped the weight.
///
/// Caught from the other end: the user reported that changing the font "doesnt visually
/// update", and the picker's own list turned out to be the same class of fault.
pub const DEFAULT_FONT_FAMILY: &str = vellum_text::BUNDLED_FAMILY;

/// Fraction of a sticky's box left as padding on each side, matching Miro's own
/// inset. Auto-fit measures against what is left.
pub const STICKY_PADDING: f64 = 0.08;

/// Which of an item's text blocks a layout belongs to.
///
/// Most items have one, but a link card has a title *and* a URL in different sizes
/// and colours, and a frame's name is laid out separately from anything inside it.
/// A slot keeps them apart without a second cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BlockKey {
    pub item: SceneId,
    /// **`u16`, not `u8`.** Most kinds use two slots, but a table uses one per cell,
    /// and 256 cells is a 16 x 16 table — a size a real board reaches.
    pub slot: u16,
}

impl BlockKey {
    /// The item's own text: a sticky's note, a text widget's body, a card's title.
    pub const PRIMARY: u16 = 0;
    /// Secondary text: a frame's name, a card's URL.
    pub const SECONDARY: u16 = 1;

    pub const fn new(item: SceneId, slot: u16) -> Self {
        Self { item, slot }
    }

    pub const fn primary(item: SceneId) -> Self {
        Self::new(item, Self::PRIMARY)
    }

    pub const fn secondary(item: SceneId) -> Self {
        Self::new(item, Self::SECONDARY)
    }
}

/// A laid-out block, and what it was laid out for.
#[derive(Debug)]
struct Entry {
    layout: Layout,
    /// The projection generation this was built against. A rebuild changes item
    /// geometry, and a sticky's auto-fit size depends on its box.
    generation: u64,
    /// The size the layout was shaped at, after auto-fit.
    font_size: f32,
}

/// Owns the font system and every live layout.
///
/// One per application: [`TextEngine::new`] enumerates the installed fonts, and both
/// of its caches only pay off when they are shared.
pub struct TextCache {
    engine: TextEngine,
    entries: HashMap<BlockKey, Entry>,
}

impl std::fmt::Debug for TextCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TextCache")
            .field("engine", &self.engine)
            .field("blocks", &self.entries.len())
            .finish()
    }
}

impl TextCache {
    /// Loads the system fonts.
    pub fn new() -> vellum_text::Result<Self> {
        Ok(Self { engine: TextEngine::new()?, entries: HashMap::new() })
    }

    /// Builds over exactly the supplied font files, with no system scan. How a test
    /// gets metrics that do not depend on the machine it runs on.
    pub fn with_fonts(fonts: impl IntoIterator<Item = Vec<u8>>) -> vellum_text::Result<Self> {
        Ok(Self { engine: TextEngine::with_fonts(fonts)?, entries: HashMap::new() })
    }

    pub fn engine_mut(&mut self) -> &mut TextEngine {
        &mut self.engine
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Drops every layout. For a document reload, where nothing carries over.
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// Drops every rasterised glyph bitmap, keeping the layouts.
    ///
    /// The layouts are zoom-independent and must survive; the bitmaps are not, and
    /// are dead the moment the zoom changes. See
    /// [`TextEngine::forget_glyph_bitmaps`].
    pub fn forget_glyph_bitmaps(&mut self) {
        self.engine.forget_glyph_bitmaps();
    }

    /// How many glyph bitmaps are resident.
    pub fn glyph_bitmaps(&self) -> usize {
        self.engine.glyph_bitmaps()
    }

    /// Drops layouts for items that are no longer on the board.
    ///
    /// Called after a rebuild rather than on every miss: an entry for a deleted item
    /// is a few hundred bytes and is never looked up again, so the only thing that
    /// matters is that it does not accumulate across a session.
    pub fn retain(&mut self, alive: impl Fn(SceneId) -> bool) {
        self.entries.retain(|key, _| alive(key.item));
    }

    /// How far text may overflow its box before a stated font size stops being honoured.
    ///
    /// **An explicit size is a choice and is normally obeyed**, even when the words spill a
    /// little — a sticky asked for 48pt keeps 48pt where 46.9 would have fitted, which
    /// `an_explicit_font_size_is_honoured_rather_than_fitted` has pinned since it was
    /// written, and Miro behaves the same way. Shrinking by a couple of percent to satisfy
    /// the box would silently restyle every deliberately-sized item on the board.
    ///
    /// What is *not* a style choice is text that needs less than half the stated size to
    /// fit — text several times too big for its own box. That is the shape of Miro's
    /// collapsed links: `docs/02-miro-formats.md` records them as **text widgets holding a
    /// bare URL**, 46 of them on the reference board, and a 500-character address with no
    /// spaces wrapped to some twenty-five lines and ran out through the bottom of its box
    /// and across the board. Reported with a screenshot of exactly that.
    ///
    /// So: obeyed up to roughly a 2× overflow, clamped past it. A threshold rather than a
    /// clean rule, and named here so the next reader can see it was chosen rather than
    /// derived.
    const RUNAWAY_TEXT_RATIO: f32 = 0.5;

    /// The laid-out block for one of an item's text slots, shaping it if the cache
    /// does not have it.
    ///
    /// `build` is only called on a miss. That matters: a card's text is assembled
    /// from several document fields, and building it on every frame of a pan would
    /// allocate a string per visible card per frame for a layout that is already
    /// cached.
    ///
    /// `fit` is the box auto-fit measures against — `None` for text that sizes
    /// itself. Returns the layout and the size it was shaped at, which the caller
    /// needs to decide whether the text is large enough on screen to be worth
    /// drawing.
    pub fn layout(
        &mut self,
        key: BlockKey,
        generation: u64,
        style: &Style,
        fit: Option<FitBox>,
        build: impl FnOnce() -> StyledText,
    ) -> (&Layout, f32) {
        let stale = self
            .entries
            .get(&key)
            .is_none_or(|entry| entry.generation != generation);

        if stale {
            let converted = build();
            let params = params_for(style, fit);
            let font_size = match style.font_size {
                // **An explicit size is a ceiling, not a command.**
                //
                // It used to be taken as-is, which meant a stated size skipped auto-fit
                // entirely and the text was drawn at that size however little room there
                // was. Miro stores a collapsed link as a *text widget* holding the bare
                // URL — `docs/02-miro-formats.md`, and 46 of the reference board's text
                // items are exactly that — so a 500-character address with no spaces
                // wrapped to some twenty-five lines and ran straight out through the
                // bottom of its box and across the board. Reported with a screenshot of
                // precisely that.
                //
                // Shrinks and never grows: `min` keeps a deliberately small size small,
                // so this only ever acts on text that does not fit. Boxless text
                // (`fit: None`) is unaffected — there is nothing to overflow.
                Some(size) if size.is_finite() && size > 0.0 => {
                    let asked = size as f32;
                    match fit {
                        Some(box_) => {
                            let fits = self
                                .engine
                                .fit_font_size(&converted, &params, box_, &AutoFit::default());
                            if fits < asked * Self::RUNAWAY_TEXT_RATIO { fits } else { asked }
                        }
                        None => asked,
                    }
                }
                // Miro's `fs: 0, fsa: 1`. Every sticky on the reference board.
                _ => match fit {
                    Some(box_) => {
                        self.engine
                            .fit_font_size(&converted, &params, box_, &AutoFit::default())
                    }
                    None => params.effective_font_size(),
                },
            };
            let params = params.with_font_size(font_size);
            let layout = self.engine.layout(&converted, &params);
            self.entries.insert(key, Entry { layout, generation, font_size });
        }

        let entry = self.entries.get(&key).expect("just inserted");
        (&entry.layout, entry.font_size)
    }

    /// Whether a block is already laid out for `generation`, so asking for it costs
    /// nothing. Lets a caller decide whether it can afford the shaping.
    pub fn is_current(&self, key: BlockKey, generation: u64) -> bool {
        self.entries
            .get(&key)
            .is_some_and(|entry| entry.generation == generation)
    }

    /// A block already laid out, without shaping anything.
    pub fn layout_of(&self, key: BlockKey) -> Option<&Layout> {
        self.entries.get(&key).map(|entry| &entry.layout)
    }

    /// Appends the atlas entries a block needs at `scale`, rasterising any glyph
    /// bitmap that is not already in the engine's cache.
    ///
    /// Takes the output buffer rather than returning a `Vec` because this runs per
    /// visible text block per frame, and it exists as a method rather than as
    /// `engine_mut().atlas_entries(layout_of(...))` at the call site because that
    /// spelling borrows the cache mutably and immutably at once.
    pub fn rasterise_into(
        &mut self,
        key: BlockKey,
        origin: (f32, f32),
        scale: f32,
        out: &mut Vec<(vellum_text::GlyphKey, vellum_text::GlyphImage)>,
    ) {
        let Some(entry) = self.entries.get(&key) else { return };
        out.extend(self.engine.atlas_entries(&entry.layout, origin, scale));
    }
}

/// Layout parameters for an item's widget-level style.
///
/// The mapping is one-to-one with Miro's own widget keys — `ffn`, `ta`, `lh` — which
/// is why it is a field copy rather than a translation.
pub fn params_for(style: &Style, fit: Option<FitBox>) -> LayoutParams {
    LayoutParams {
        font_family: Some(
            style
                .font_family
                .clone()
                .unwrap_or_else(|| DEFAULT_FONT_FAMILY.to_string()),
        ),
        font_size: style.font_size.unwrap_or(vellum_text::DEFAULT_FONT_SIZE as f64) as f32,
        line_height: style.line_height.unwrap_or(vellum_text::DEFAULT_LINE_HEIGHT as f64) as f32,
        align: match style.align {
            Some(Align::Center) => TextAlign::Center,
            Some(Align::Right) => TextAlign::Right,
            _ => TextAlign::Left,
        },
        max_width: fit.map(|b| b.width),
    }
}

/// Converts the document's styled text into the text engine's.
///
/// The two types are deliberate twins — `docs/01-architecture.md` §2 puts the
/// dependency arrow at `doc → text`, so the text engine cannot know about the CRDT
/// and the CRDT will not pull one in. That leaves exactly one place for the
/// conversion, and this is it.
pub fn convert(text: &DocText) -> StyledText {
    StyledText::from_spans(text.spans().iter().map(|span| {
        TextSpan::new(
            span.text.clone(),
            SpanStyle {
                bold: span.style.bold,
                italic: span.style.italic,
                underline: span.style.underline,
                strikethrough: span.style.strikethrough,
                link: span.style.link.clone(),
                // Alpha is dropped rather than the colour: a translucent glyph run
                // is not something Miro's editor can produce, and `vellum_text::Rgb`
                // has nowhere to put it.
                color: span.style.color.map(|c| Rgb::new(c.r, c.g, c.b)),
            },
        )
    }))
}

/// The box a sticky's text is fitted into: its rectangle less Miro's padding.
pub fn sticky_fit(width: f64, height: f64) -> FitBox {
    FitBox::new(
        (width * (1.0 - STICKY_PADDING * 2.0)).max(1.0) as f32,
        (height * (1.0 - STICKY_PADDING * 2.0)).max(1.0) as f32,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use vellum_doc::{Color, SpanStyle as DocSpanStyle, TextSpan as DocSpan};

    fn cache() -> TextCache {
        TextCache::new().expect("the test machine has fonts")
    }

    fn sticky_text() -> DocText {
        DocText::plain("fan control module")
    }

    #[test]
    fn spans_survive_the_conversion_with_their_formatting() {
        let doc = DocText::from_spans([
            DocSpan::plain("plain "),
            DocSpan::new(
                "bold",
                DocSpanStyle { bold: true, ..DocSpanStyle::default() },
            ),
            DocSpan::new(
                " red",
                DocSpanStyle {
                    color: Some(Color::rgb(0xE6, 0x5B, 0x58)),
                    ..DocSpanStyle::default()
                },
            ),
        ]);

        let converted = convert(&doc);
        assert_eq!(converted.to_plain(), "plain bold red");
        assert_eq!(converted.spans().len(), 3);
        assert!(converted.spans()[1].style.bold);
        assert_eq!(converted.spans()[2].style.color, Some(Rgb::new(0xE6, 0x5B, 0x58)));
    }

    #[test]
    fn a_link_span_keeps_its_target() {
        let doc = DocText::from_spans([DocSpan::new(
            "spec sheet",
            DocSpanStyle::link("https://example.com"),
        )]);
        assert_eq!(
            convert(&doc).spans()[0].style.link.as_deref(),
            Some("https://example.com")
        );
    }

    /// Auto-fit is what a sticky uses when `font_size` is `None`, and the size it
    /// picks must actually fit the note.
    #[test]
    fn auto_fit_sizes_the_text_to_the_note() {
        let mut cache = cache();
        let fit = sticky_fit(199.0, 228.0);
        let (layout, size) = cache.layout(BlockKey::primary(1), 0, &Style::default(), Some(fit), || {
            convert(&sticky_text())
        });

        assert!(size > 1.0, "auto-fit collapsed to {size}");
        assert!(layout.extent.width <= fit.width + 1.0, "{:?}", layout.extent);
        assert!(layout.extent.height <= fit.height + 1.0, "{:?}", layout.extent);
    }

    /// A smaller note gets smaller text. Without this the auto-fit is not doing
    /// anything and every sticky renders at the default size.
    #[test]
    fn a_smaller_note_fits_smaller_text() {
        let mut cache = cache();
        let build = || convert(&sticky_text());
        let large = cache
            .layout(BlockKey::primary(1), 0, &Style::default(), Some(sticky_fit(400.0, 400.0)), build)
            .1;
        let small = cache
            .layout(BlockKey::primary(2), 0, &Style::default(), Some(sticky_fit(80.0, 80.0)), build)
            .1;
        assert!(small < large, "{small} was not smaller than {large}");
    }

    #[test]
    fn an_explicit_font_size_is_honoured_rather_than_fitted() {
        let mut cache = cache();
        let style = Style { font_size: Some(48.0), ..Style::default() };
        let (_, size) = cache.layout(
            BlockKey::primary(1),
            0,
            &style,
            Some(sticky_fit(199.0, 228.0)),
            || convert(&sticky_text()),
        );
        assert_eq!(size, 48.0);
    }

    /// A bare URL in a small box is clamped, where a merely-tight fit is not.
    ///
    /// The pair matters more than either half. `an_explicit_font_size_is_honoured_rather_
    /// than_fitted` above asserts that a 48pt choice survives a box that would prefer
    /// 46.9 — and a fix for the runaway case that ignored it shrank **every**
    /// deliberately-sized item on the board by a couple of percent. The two tests
    /// together say where the line is: obeyed when the text nearly fits, overridden when
    /// it needs less than half the stated size.
    ///
    /// The fixture is the real shape of the defect — one unbreakable token far longer
    /// than the box, which is how Miro stores a collapsed link.
    #[test]
    fn a_runaway_url_is_clamped_but_a_tight_fit_is_not() {
        let mut cache = cache();
        let style = Style { font_size: Some(48.0), ..Style::default() };
        let url = "https://www.aliexpress.us/item/1005000000000.html?spm=a2g0o.\
                   productlist.main.24.3a1528b0RjmIX8&algo_pvid=15464176-1975-43d9-9c23-\
                   15d6c868609f&algo_exp_id=15464176-1975-43d9-9c23-15d6c868609f-0";

        let (_, runaway) = cache.layout(
            BlockKey::primary(1),
            0,
            &style,
            Some(sticky_fit(199.0, 228.0)),
            || convert(&DocText::plain(url)),
        );
        assert!(
            runaway < 48.0,
            "a {}-character URL kept its 48pt and ran out of the box at {runaway}",
            url.len()
        );

        // …and the ordinary case is untouched: a few words at 48pt in the same box.
        let (_, ordinary) = cache.layout(
            BlockKey::primary(2),
            0,
            &style,
            Some(sticky_fit(199.0, 228.0)),
            || convert(&sticky_text()),
        );
        assert_eq!(ordinary, 48.0, "a tight but ordinary fit was restyled");
    }

    /// The whole point of the cache: a second frame reuses the layout rather than
    /// re-shaping and re-fitting it — and does not even build the text to do so.
    #[test]
    fn a_layout_is_computed_once_per_generation() {
        let mut cache = cache();
        let fit = Some(sticky_fit(199.0, 228.0));
        let mut builds = 0;

        let first = cache
            .layout(BlockKey::primary(1), 7, &Style::default(), fit, || {
                builds += 1;
                convert(&sticky_text())
            })
            .1;
        assert_eq!(builds, 1);
        assert_eq!(cache.len(), 1);

        let second = cache
            .layout(BlockKey::primary(1), 7, &Style::default(), fit, || {
                builds += 1;
                convert(&DocText::plain("something else"))
            })
            .1;
        assert_eq!(second, first);
        assert_eq!(builds, 1, "a cache hit still built its text");

        let third = cache
            .layout(BlockKey::primary(1), 8, &Style::default(), fit, || {
                builds += 1;
                convert(&DocText::plain("a very much longer string than before"))
            })
            .1;
        assert_eq!(builds, 2);
        assert_ne!(third, first, "a new generation did not re-shape");
    }

    /// Two blocks on one item — a card's title and its URL — must not evict each
    /// other.
    #[test]
    fn an_items_slots_are_cached_separately() {
        let mut cache = cache();
        cache.layout(BlockKey::primary(1), 0, &Style::default(), None, || {
            convert(&DocText::plain("title"))
        });
        cache.layout(BlockKey::secondary(1), 0, &Style::default(), None, || {
            convert(&DocText::plain("https://example.com"))
        });
        assert_eq!(cache.len(), 2);
    }

    /// The signal a caller uses to decide whether shaping is affordable this frame.
    #[test]
    fn a_block_is_current_only_for_the_generation_it_was_shaped_at() {
        let mut cache = cache();
        let key = BlockKey::primary(1);
        assert!(!cache.is_current(key, 0));

        cache.layout(key, 0, &Style::default(), None, || convert(&sticky_text()));
        assert!(cache.is_current(key, 0));
        assert!(!cache.is_current(key, 1), "a rebuild did not invalidate the block");
        assert!(!cache.is_current(BlockKey::secondary(1), 0));
    }

    #[test]
    fn dead_items_are_forgotten() {
        let mut cache = cache();
        for id in 0..5 {
            cache.layout(BlockKey::primary(id), 0, &Style::default(), None, || {
                convert(&sticky_text())
            });
        }
        assert_eq!(cache.len(), 5);
        cache.retain(|id| id < 2);
        assert_eq!(cache.len(), 2);
        cache.clear();
        assert!(cache.is_empty());
    }

    #[test]
    fn widget_style_maps_onto_layout_parameters() {
        let style = Style {
            font_family: Some("Helvetica".into()),
            font_size: Some(22.0),
            line_height: Some(1.5),
            align: Some(Align::Center),
            ..Style::default()
        };
        let params = params_for(&style, Some(FitBox::new(300.0, 100.0)));

        assert_eq!(params.font_family.as_deref(), Some("Helvetica"));
        assert_eq!(params.font_size, 22.0);
        assert_eq!(params.line_height, 1.5);
        assert_eq!(params.align, TextAlign::Center);
        assert_eq!(params.max_width, Some(300.0));
    }

    /// Every widget on an imported Miro board names this family. Falling back to the
    /// platform sans would move every wrap point on the board.
    #[test]
    fn an_unstyled_item_asks_for_miros_family() {
        let params = params_for(&Style::default(), None);
        assert_eq!(params.font_family.as_deref(), Some(DEFAULT_FONT_FAMILY));
        assert_eq!(params.max_width, None, "text with no box must not wrap");
    }

    #[test]
    fn sticky_padding_shrinks_the_fit_box_symmetrically() {
        let fit = sticky_fit(200.0, 100.0);
        assert!((fit.width - 168.0).abs() < 1e-3, "{}", fit.width);
        assert!((fit.height - 84.0).abs() < 1e-3, "{}", fit.height);
        // A degenerate item must not produce a zero or negative box, which auto-fit
        // cannot converge against.
        assert!(sticky_fit(0.0, 0.0).width >= 1.0);
    }
}
