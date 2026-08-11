//! Shaping and layout: styled spans in, positioned glyphs out.
//!
//! `cosmic-text` does the hard parts — rustybuzz shaping, bidi, script itemisation,
//! font fallback, line breaking. What this module adds is everything between it and
//! the rest of Vellum:
//!
//! - Vellum's [`StyledText`] becomes cosmic-text's rich-text spans, with each span's
//!   index carried through shaping so a run's underline, strike and link survive
//!   into the output. cosmic-text itself models colour and weight but has no notion
//!   of a decoration or a hyperlink.
//! - The output is flattened into owned, block-relative [`PlacedGlyph`]s. A
//!   `cosmic_text::Buffer` is a mutable, cached, borrow-heavy object; the renderer
//!   wants a value it can hold across frames and hand to a spatial index.
//! - [`TextEngine::measure`] answers "how big is this" without touching the
//!   rasteriser, because auto-fit calls it about a dozen times per sticky.
//!
//! ## cosmic-text 0.14 behaviours worth knowing
//!
//! **A `Buffer`'s height clips its own layout.** `LayoutRunIter` stops as soon as a
//! line's baseline passes `height_opt`, so a buffer given the widget's height
//! reports the height of the *visible* text, not of the text. Every buffer here is
//! therefore built with `height_opt = None` and clipped by the caller — without
//! that, auto-fit converges on "any size at which the overflow is invisible".
//!
//! **Vertical position is hinted, horizontal is not.** `CacheKey` bins the
//! fractional x into quarters but truncates y outright, so glyphs snap to integer
//! baselines. [`PlacedGlyph::physical`] reproduces that exactly rather than
//! inventing its own rounding, so a glyph rasterised here matches one rasterised by
//! `Buffer::draw`.

use crate::error::{Result, TextError};
use crate::span::{Rgb, SpanStyle, StyledText};
use cosmic_text::{
    Align, Attrs, Buffer, CacheKey, CacheKeyFlags, Color, Family, FontSystem, Metrics, Shaping,
    Style, SwashCache, Weight, Wrap, fontdb,
};
use std::ops::Range;

/// Miro's default line height, `lh: 1.36` in the compact style keys.
pub const DEFAULT_LINE_HEIGHT: f32 = 1.36;

/// Used when a caller supplies a size that cannot be laid out at all (zero,
/// negative, NaN). Matches Miro's `fs: 14` on frame titles.
pub const DEFAULT_FONT_SIZE: f32 = 14.0;

/// Font sizes are clamped into this range before they reach cosmic-text, which
/// asserts on a zero line height and produces degenerate outlines near zero. A
/// widget whose style says `fs: 0` means *auto-fit* (see [`crate::AutoFit`]) and
/// must be resolved before layout; reaching here with it is a caller bug, and
/// clamping reports it as unreadable text rather than as a panic in the renderer.
pub const MIN_FONT_SIZE: f32 = 0.25;
/// Upper clamp, well past any size a board uses; it exists to keep a corrupt style
/// value from asking the rasteriser for a gigapixel glyph.
pub const MAX_FONT_SIZE: f32 = 4096.0;

/// Identifies a font face after fallback resolution.
///
/// This is `fontdb`'s id, reached through cosmic-text's re-export. Vellum does not
/// depend on `fontdb` directly: a second, independently versioned copy of it in the
/// tree would produce two incompatible `ID` types for the same face.
pub type FontId = fontdb::ID;

/// Horizontal alignment, mirroring Miro's `ta` key and `vellum_doc::Align`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum TextAlign {
    #[default]
    Left,
    Center,
    Right,
}

impl TextAlign {
    fn to_cosmic(self) -> Align {
        match self {
            Self::Left => Align::Left,
            Self::Center => Align::Center,
            Self::Right => Align::Right,
        }
    }
}

/// Everything about a text block that is not per-span.
///
/// These are exactly Miro's widget-level style keys, so an imported widget maps
/// across without an intermediate representation: `ffn` → [`font_family`],
/// `fs` → [`font_size`], `lh` → [`line_height`], `ta` → [`align`].
///
/// [`font_family`]: Self::font_family
/// [`font_size`]: Self::font_size
/// [`line_height`]: Self::line_height
/// [`align`]: Self::align
#[derive(Debug, Clone, PartialEq)]
pub struct LayoutParams {
    /// Family name, e.g. Miro's `"Noto Sans"`. `None` uses the platform sans-serif.
    pub font_family: Option<String>,
    /// Size in px, in the same space as [`max_width`](Self::max_width).
    pub font_size: f32,
    /// Line height as a *multiple* of the font size, matching Miro's `lh`.
    pub line_height: f32,
    pub align: TextAlign,
    /// Wrap width. `None` lays every paragraph out on one line, which is what
    /// measuring a widget's natural width needs.
    pub max_width: Option<f32>,
}

impl Default for LayoutParams {
    fn default() -> Self {
        Self {
            font_family: None,
            font_size: DEFAULT_FONT_SIZE,
            line_height: DEFAULT_LINE_HEIGHT,
            align: TextAlign::Left,
            max_width: None,
        }
    }
}

impl LayoutParams {
    pub fn with_font_size(&self, font_size: f32) -> Self {
        Self { font_size, ..self.clone() }
    }

    pub fn with_max_width(&self, max_width: Option<f32>) -> Self {
        Self { max_width, ..self.clone() }
    }

    /// The size actually used, after clamping to [`MIN_FONT_SIZE`]/[`MAX_FONT_SIZE`].
    pub fn effective_font_size(&self) -> f32 {
        if self.font_size.is_finite() {
            self.font_size.clamp(MIN_FONT_SIZE, MAX_FONT_SIZE)
        } else {
            DEFAULT_FONT_SIZE
        }
    }

    /// Distance between consecutive baselines, in px.
    pub fn effective_line_height(&self) -> f32 {
        let factor = if self.line_height.is_finite() && self.line_height > 0.0 {
            self.line_height
        } else {
            DEFAULT_LINE_HEIGHT
        };
        (self.effective_font_size() * factor).max(MIN_FONT_SIZE)
    }

    fn metrics(&self) -> Metrics {
        Metrics::new(self.effective_font_size(), self.effective_line_height())
    }

    /// A wrap width is only meaningful if it is positive and finite; anything else
    /// means "do not wrap" rather than "wrap at nothing".
    fn wrap_width(&self) -> Option<f32> {
        self.max_width.filter(|w| w.is_finite() && *w > 0.0)
    }
}

/// The bounding box of a laid-out text block, without any glyph data.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TextExtent {
    /// Widest line. Never exceeds the wrap width except where a single glyph is
    /// wider than it and there is nowhere left to break.
    pub width: f32,
    /// Total height: `lines × line_height`, so it stays a whole number of lines
    /// even when the last line's glyphs are short.
    pub height: f32,
    /// Visual lines after wrapping — always at least one, so an empty widget still
    /// reserves a caret's worth of space.
    pub lines: usize,
}

/// One visual line, after wrapping.
#[derive(Debug, Clone, PartialEq)]
pub struct LaidOutLine {
    /// Which **paragraph** of the source this visual line belongs to — cosmic-text's
    /// `line_i`. A paragraph is a run between `\n`s and may wrap into several visual
    /// lines, so this is not the line's own index.
    ///
    /// Recorded because [`PlacedGlyph::cluster`] is a byte range *within its paragraph*,
    /// which is useless for addressing a caret in the whole string without knowing which
    /// paragraph it is in. See [`Layout::caret`].
    pub paragraph: usize,
    /// Top of the line box, relative to the top of the block.
    pub top: f32,
    /// Baseline, relative to the top of the block. Glyph positions are absolute
    /// within the block, so this is for drawing decorations, not for offsetting.
    pub baseline: f32,
    pub height: f32,
    /// Advance width of the glyphs on this line, before alignment.
    pub width: f32,
    /// Glyphs in visual order, already bidi-reordered.
    pub glyphs: Vec<PlacedGlyph>,
}

/// A shaped, positioned glyph.
///
/// Positions are block-relative and in logical px: the renderer applies the camera
/// transform, and the same layout is reusable at every zoom level. Turning one into
/// something the GPU can index needs a pixel grid, which is [`Self::physical`].
#[derive(Debug, Clone, PartialEq)]
pub struct PlacedGlyph {
    /// Pen position, from the left edge of the block.
    pub x: f32,
    /// Baseline this glyph sits on, from the top of the block.
    pub y: f32,
    /// Advance width.
    pub advance: f32,
    pub font_size: f32,
    /// Shaping offsets (kerning, mark attachment) in *ems*, applied on top of
    /// `x`/`y` at rasterisation time — cosmic-text's own convention, kept so the
    /// arithmetic in [`Self::physical`] matches `Buffer::draw` exactly.
    pub x_offset: f32,
    pub y_offset: f32,
    /// Face chosen for this glyph, which is not the requested family when fallback
    /// ran — an emoji or a CJK glyph in a Latin run.
    pub font: FontId,
    pub glyph_id: u16,
    /// Colour override carried down from the span.
    pub color: Option<Rgb>,
    /// Index into the [`StyledText`] this layout came from, or `None` for a glyph on
    /// a line that carried no span (an empty line). This is what lets a renderer
    /// recover underline, strike and link, none of which cosmic-text models.
    pub span: Option<usize>,
    /// Byte range of the source cluster within its *paragraph*, which is what
    /// caret placement and hit-testing index by.
    pub cluster: Range<usize>,
    /// Synthetic-italic state. Private because it is cosmic-text's affair: it
    /// belongs to the cache key, not to Vellum's model of a glyph.
    flags: CacheKeyFlags,
}

/// A glyph snapped to the pixel grid, with the key its bitmap is cached under.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PhysicalGlyph {
    pub key: GlyphKey,
    /// Integer device-pixel position of the bitmap's origin. The bitmap's own
    /// `left`/`top` offsets are added on top — see [`crate::GlyphImage`].
    pub x: i32,
    pub y: i32,
}

/// Identifies one rasterised glyph image: face, glyph, size and subpixel phase.
///
/// This is the key a GPU atlas is indexed by, so it is `Copy + Eq + Hash + Ord` and
/// nothing more — deliberately opaque, because its *contents* are cosmic-text's
/// cache identity and changing them must not be a breaking change here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct GlyphKey(pub(crate) CacheKey);

impl GlyphKey {
    pub fn font(self) -> FontId {
        self.0.font_id
    }

    pub fn glyph_id(self) -> u16 {
        self.0.glyph_id
    }

    /// Device-pixel size this glyph is rasterised at, i.e. the logical size already
    /// multiplied by the zoom the key was built with.
    pub fn font_size(self) -> f32 {
        f32::from_bits(self.0.font_size_bits)
    }

    /// Fractional pen offset the bitmap was rendered for, in quarter pixels.
    pub fn subpixel_offset(self) -> (f32, f32) {
        (self.0.x_bin.as_float(), self.0.y_bin.as_float())
    }
}

impl PlacedGlyph {
    /// Snaps this glyph to the pixel grid for a block drawn at `origin` with `scale`
    /// device pixels per logical px.
    ///
    /// `scale` folds zoom and DPI together: a glyph is rasterised at the size it
    /// will occupy on screen, which is why the key carries a size at all. Callers
    /// that redraw continuously while zooming should quantise `scale` before
    /// calling, or every frame mints a whole new atlas.
    pub fn physical(&self, origin: (f32, f32), scale: f32) -> PhysicalGlyph {
        let x = (self.x + self.font_size * self.x_offset) * scale + origin.0;
        // Truncation, not rounding: cosmic-text hints the vertical axis so that a
        // baseline always lands on a pixel boundary and horizontal stems stay
        // crisp. Rounding here instead would make our glyphs disagree with the
        // ones `Buffer::draw` produces at the same position.
        let y = ((self.y - self.font_size * self.y_offset) * scale + origin.1).trunc();
        let (key, x, y) = CacheKey::new(
            self.font,
            self.glyph_id,
            self.font_size * scale,
            (x, y),
            self.flags,
        );
        PhysicalGlyph { key: GlyphKey(key), x, y }
    }
}

/// A run of glyphs sharing one text decoration.
///
/// Underline and strikethrough are not glyphs — nothing shapes them, so they arrive
/// as spans of formatting and leave as spans of geometry. The vertical offset and
/// thickness are deliberately *not* here: both come from the face's own
/// `underline_position`/`underline_thickness` metrics, which the renderer has and
/// this crate would have to guess at.
#[derive(Debug, Clone, PartialEq)]
pub struct DecorationRun {
    pub decoration: Decoration,
    /// Index into [`Layout::lines`].
    pub line: usize,
    pub x: f32,
    pub width: f32,
    /// Baseline the run sits on, relative to the top of the block.
    pub baseline: f32,
    /// Largest font size in the run, which is the scale its thickness derives from.
    pub font_size: f32,
    pub color: Option<Rgb>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Decoration {
    Underline,
    Strikethrough,
}

/// A laid-out text block.
#[derive(Debug, Clone, PartialEq)]
pub struct Layout {
    pub lines: Vec<LaidOutLine>,
    pub extent: TextExtent,
}

/// Where a caret sits in a laid-out block, in block-relative logical px.
///
/// A zero-width position, not a rectangle: the *drawn* caret's width is the renderer's
/// business — it wants one device pixel at any zoom, which this has no way to know.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Caret {
    /// Index into [`Layout::lines`] — a **visual** line, so a wrapped paragraph has
    /// several.
    pub line: usize,
    /// Distance from the left edge of the block to the caret.
    pub x: f32,
    /// Top of the line box the caret sits in.
    pub top: f32,
    /// Height of that line box. The caret spans the line, not the glyph: a caret as tall
    /// as whatever character happens to be beside it jumps as you move it through mixed
    /// sizes.
    pub height: f32,
}

/// One visual line's worth of a selection, in block-relative logical px.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SelectionBox {
    pub line: usize,
    pub x: f32,
    pub width: f32,
    pub top: f32,
    pub height: f32,
}

/// Byte offset where each paragraph of `text` begins.
///
/// A paragraph is a run between `\n`s, which is what cosmic-text's `BidiParagraphs`
/// splits on and therefore what [`LaidOutLine::paragraph`] counts. Always at least one
/// entry, because the empty string is one empty paragraph — and a caret has to be able to
/// sit in it.
fn paragraph_starts(text: &str) -> Vec<usize> {
    let mut starts = vec![0];
    starts.extend(text.match_indices('\n').map(|(at, _)| at + 1));
    starts
}

impl Layout {
    /// Every glyph in the block, in reading order.
    pub fn glyphs(&self) -> impl Iterator<Item = &PlacedGlyph> {
        self.lines.iter().flat_map(|line| line.glyphs.iter())
    }

    /// Where the caret goes for a byte offset into `text`.
    ///
    /// `text` is the plain string the layout was built from — the same one the offset
    /// indexes. It is a parameter rather than a field because [`PlacedGlyph::cluster`] is
    /// a byte range **within its paragraph**, so resolving a whole-string offset needs the
    /// paragraph boundaries, and those are a property of the string rather than of the
    /// layout. Keeping a copy of the text on every `Layout` would put a second owner of
    /// the item's words in the hot per-frame structure.
    ///
    /// An offset past the end of the text clamps to the end; one inside a multi-byte
    /// character resolves to the start of the cluster containing it. Neither is a caller
    /// error worth an `Option` — a caret is a position on screen, and there is always one.
    ///
    /// # Honest limits
    ///
    /// **Logical order is assumed to be visual order**, so this is correct for LTR text
    /// and wrong inside an RTL run: `cluster` recovers *which* character a glyph came
    /// from, but `PlacedGlyph` does not carry its bidi level, so there is no way here to
    /// tell "the caret before this character" from "the caret after it" when the run
    /// reverses. Mixed-direction text needs the level plumbed through shaping first.
    pub fn caret(&self, text: &str, byte: usize) -> Caret {
        let byte = byte.min(text.len());
        let starts = paragraph_starts(text);
        let fallback = Caret {
            line: 0,
            x: 0.0,
            top: 0.0,
            height: self.extent.height.max(1.0),
        };
        let Some(paragraph) = starts.iter().rposition(|start| *start <= byte) else {
            return fallback;
        };

        // The visual lines of that paragraph, and the one the offset falls on. A wrapped
        // paragraph has several; the offset belongs to the last line whose first cluster
        // starts at or before it, which puts a caret at a soft break on the *following*
        // line — the convention every editor uses, because that is where the next
        // character will appear.
        let mut chosen: Option<(usize, &LaidOutLine)> = None;
        for (index, line) in self.lines.iter().enumerate() {
            if line.paragraph != paragraph {
                continue;
            }
            let local = byte - starts[paragraph];
            let begins_at = line.glyphs.first().map_or(0, |g| g.cluster.start);
            if chosen.is_none() || begins_at <= local {
                chosen = Some((index, line));
            }
        }
        let Some((index, line)) = chosen else { return fallback };

        let local = byte - starts[paragraph];
        // The glyph the caret sits *before*, if there is one on this line.
        let x = line
            .glyphs
            .iter()
            .find(|glyph| glyph.cluster.start >= local)
            .map_or_else(
                // Past every glyph on the line: the caret goes after the last one. Not
                // `line.width`, which is the advance width *before* alignment and would
                // put the caret at the left edge of a centred line.
                || {
                    line.glyphs
                        .iter()
                        .map(|glyph| glyph.x + glyph.advance)
                        .fold(0.0_f32, f32::max)
                },
                |glyph| glyph.x,
            );

        Caret { line: index, x, top: line.top, height: line.height }
    }

    /// The byte offset nearest a block-relative point — where a click puts the caret.
    ///
    /// Clamped in both axes: a click above the block lands at its start and one below at
    /// its end, which is what dragging a selection off the top of a paragraph has to do.
    /// Within a line, the offset is the nearer *edge* of the glyph under the pointer, so
    /// clicking the right half of a character puts the caret after it.
    ///
    /// Same LTR assumption as [`Self::caret`].
    pub fn byte_at(&self, text: &str, x: f32, y: f32) -> usize {
        let starts = paragraph_starts(text);
        if self.lines.is_empty() {
            return 0;
        }
        // The line whose box contains y, or the nearest one.
        let line = self
            .lines
            .iter()
            .position(|line| y >= line.top && y < line.top + line.height)
            .unwrap_or_else(|| if y < self.lines[0].top { 0 } else { self.lines.len() - 1 });
        let line = &self.lines[line];
        let start = starts.get(line.paragraph).copied().unwrap_or(0);
        let end_of_paragraph = starts
            .get(line.paragraph + 1)
            // Past the separator, so the offset stays inside this paragraph.
            .map_or(text.len(), |next| next.saturating_sub(1));

        let mut best = line.glyphs.first().map_or(end_of_paragraph, |g| start + g.cluster.start);
        for glyph in &line.glyphs {
            // The right edge wins from the middle of the glyph onwards.
            if x >= glyph.x + glyph.advance * 0.5 {
                best = (start + glyph.cluster.end).min(end_of_paragraph);
            } else {
                best = start + glyph.cluster.start;
                break;
            }
        }
        best.min(text.len())
    }

    /// One box per visual line covered by `range`, for painting a selection.
    ///
    /// An empty range produces nothing rather than a zero-width box: a caret is drawn by
    /// [`Self::caret`], and a selection highlight of no width is a caret drawn twice.
    pub fn selection_boxes(&self, text: &str, range: Range<usize>) -> Vec<SelectionBox> {
        let (from, to) = (range.start.min(range.end), range.start.max(range.end));
        if from >= to {
            return Vec::new();
        }
        let starts = paragraph_starts(text);
        let mut boxes = Vec::new();
        for (index, line) in self.lines.iter().enumerate() {
            let start = starts.get(line.paragraph).copied().unwrap_or(0);
            // Every glyph on this line that the range covers. Taken glyph by glyph rather
            // than as "the leftmost and rightmost", so a line whose covered glyphs are
            // not contiguous — which bidi produces — still highlights only what is
            // selected, in as many boxes as it takes.
            let mut span: Option<(f32, f32)> = None;
            for glyph in &line.glyphs {
                let (gs, ge) = (start + glyph.cluster.start, start + glyph.cluster.end);
                if ge <= from || gs >= to {
                    // Not covered. Close any run that was open, so a gap becomes two
                    // boxes rather than one that spans it.
                    if let Some((left, right)) = span.take() {
                        boxes.push(SelectionBox {
                            line: index,
                            x: left,
                            width: right - left,
                            top: line.top,
                            height: line.height,
                        });
                    }
                    continue;
                }
                let (left, right) = (glyph.x, glyph.x + glyph.advance);
                span = Some(match span {
                    Some((l, r)) => (l.min(left), r.max(right)),
                    None => (left, right),
                });
            }
            if let Some((left, right)) = span {
                boxes.push(SelectionBox {
                    line: index,
                    x: left,
                    width: right - left,
                    top: line.top,
                    height: line.height,
                });
            }
        }
        boxes
    }

    /// Groups glyphs into the underline and strikethrough runs `text` calls for.
    ///
    /// Takes the source text rather than caching the styles on each glyph: a
    /// decoration is a property of a *span*, and duplicating it per glyph would put
    /// a second copy of the formatting in the hot layout structure.
    pub fn decoration_runs(&self, text: &StyledText) -> Vec<DecorationRun> {
        let mut runs = Vec::new();
        for (index, line) in self.lines.iter().enumerate() {
            for decoration in [Decoration::Underline, Decoration::Strikethrough] {
                collect_decoration(&mut runs, index, line, text, decoration);
            }
        }
        runs
    }
}

fn collect_decoration(
    runs: &mut Vec<DecorationRun>,
    line_index: usize,
    line: &LaidOutLine,
    text: &StyledText,
    decoration: Decoration,
) {
    let mut open: Option<DecorationRun> = None;
    for glyph in &line.glyphs {
        let style = glyph.span.and_then(|i| text.spans().get(i)).map(|span| &span.style);
        let wanted = style.is_some_and(|style| match decoration {
            Decoration::Underline => style.underline,
            Decoration::Strikethrough => style.strikethrough,
        });

        let colour = style.and_then(|style| style.color);
        match (&mut open, wanted) {
            // Extend only while the colour matches: a run that changes colour
            // mid-way must be drawn as two rules, not one in the wrong colour.
            (Some(run), true) if run.color == colour => {
                run.width = glyph.x + glyph.advance - run.x;
                run.font_size = run.font_size.max(glyph.font_size);
            }
            (_, true) => {
                if let Some(run) = open.take() {
                    runs.push(run);
                }
                open = Some(DecorationRun {
                    decoration,
                    line: line_index,
                    x: glyph.x,
                    width: glyph.advance,
                    baseline: line.baseline,
                    font_size: glyph.font_size,
                    color: colour,
                });
            }
            (_, false) => {
                if let Some(run) = open.take() {
                    runs.push(run);
                }
            }
        }
    }
    runs.extend(open);
}

/// Owns the font database and the glyph caches.
///
/// One per application, not one per widget: `FontSystem::new` enumerates every
/// installed font, and both caches only pay off when shared. It is `!Sync` in
/// practice — put it behind the same lock as the renderer rather than cloning it.
pub struct TextEngine {
    pub(crate) fonts: FontSystem,
    pub(crate) glyphs: SwashCache,
}

impl std::fmt::Debug for TextEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TextEngine").field("faces", &self.fonts.db().len()).finish()
    }
}

/// The family the bundled faces belong to, and what `sans-serif` is pointed at.
pub const BUNDLED_FAMILY: &str = "Inter";

/// Inter Regular and Bold, compiled into the binary.
///
/// # Why a font is shipped rather than borrowed from the machine
///
/// `font_family: None` — which is every item on every board that did not name one —
/// resolves through fontdb's `sans-serif` alias, and on this machine that reached **Noto
/// Sans, regular weight only**. cosmic-text has no synthetic bold, so
/// [`TextEngine::family_has_bold`] correctly refused to ask for a weight the family had not
/// got, and **every bold span in the application silently shaped at regular weight** — link
/// card titles included, which is why Miro's cards read from a distance and Velm's did not.
/// The user asked for the titles to be legible the way Miro's are and chose bundling over
/// pointing the alias at whatever the host happens to have.
///
/// Bundling is the durable answer for a reason beyond bold: a board is a document that has
/// to lay out the same tomorrow. Text metrics decide wrapping, auto-fit sizes and — for a
/// mind map — the *geometry of the tree*, so a board shaped against a font the machine
/// happened to have would re-flow on a machine that has a different one, and would not match
/// its own exported SVG. Two static faces at ~410KB each is the price of that.
///
/// Static Regular and Bold rather than `InterVariable.ttf`: fontdb matches weights across
/// *faces*, and a variable font presents one face whose named instances it does not expose
/// as separate weights — so the variable file would reintroduce exactly the bug this fixes,
/// at a smaller download.
///
/// Licensed under the SIL Open Font License 1.1; the licence ships beside the files in
/// `assets/fonts/Inter-LICENSE.txt` and is copied into the bundle by `scripts/make-app.sh`.
pub const BUNDLED_FONTS: [&[u8]; 2] = [
    include_bytes!("../../../assets/fonts/Inter-Regular.ttf"),
    include_bytes!("../../../assets/fonts/Inter-Bold.ttf"),
];

impl TextEngine {
    /// The system font set, plus the bundled family, with `sans-serif` pointed at it.
    ///
    /// The system fonts stay loaded: a Miro board names real families in its `ffn` spans
    /// (`Arial`, `Times New Roman`), and dropping them would substitute the bundled face for
    /// a typeface the user deliberately chose. What the bundle changes is only the
    /// *default* — see [`BUNDLED_FONTS`].
    ///
    /// # Errors
    /// [`TextError::NoFontsAvailable`] when the platform reports no fonts at all.
    pub fn new() -> Result<Self> {
        let mut fonts = FontSystem::new();
        for face in BUNDLED_FONTS {
            fonts.db_mut().load_font_data(face.to_vec());
        }
        // **This line is the fix, not the `load_font_data` above it.** Loading a bold face
        // the alias does not point at changes nothing: `family_has_bold(None)` asks what
        // `sans-serif` resolves to and counts the faces of *that* family, so without this the
        // bundle would sit in the database, unreferenced, and every bold span would still
        // shape at regular weight.
        fonts.db_mut().set_sans_serif_family(BUNDLED_FAMILY);
        Self::from_font_system(fonts)
    }

    /// Builds an engine over exactly the supplied font files, with no system scan.
    ///
    /// This is how a test gets reproducible metrics, and how Vellum will ship Noto
    /// Sans — the family every Miro board names in `ffn` — rather than hoping it is
    /// installed.
    ///
    /// # Errors
    /// [`TextError::NoFontsAvailable`] when none of the data parses as a font.
    pub fn with_fonts(fonts: impl IntoIterator<Item = Vec<u8>>) -> Result<Self> {
        let sources = fonts
            .into_iter()
            .map(|data| fontdb::Source::Binary(std::sync::Arc::new(data)))
            .collect::<Vec<_>>();
        Self::from_font_system(FontSystem::new_with_fonts(sources))
    }

    fn from_font_system(fonts: FontSystem) -> Result<Self> {
        if fonts.db().is_empty() {
            return Err(TextError::NoFontsAvailable);
        }
        Ok(Self { fonts, glyphs: SwashCache::new() })
    }

    /// Adds a font from memory to an existing engine.
    pub fn load_font(&mut self, data: Vec<u8>) {
        self.fonts.db_mut().load_font_data(data);
    }

    /// Every family the shaper can actually use, sorted and deduplicated.
    ///
    /// Exists so the font picker can only offer families that will *do* something. Its list
    /// was hardcoded and included `Segoe UI` and `Cascadia Mono` — Windows fonts, absent on
    /// macOS — so picking one changed the stored value, changed nothing on screen, and read as
    /// the control being broken. Reported exactly that way: *"when i change the font somewhere
    /// all it does it update but it doesnt visually update."*
    ///
    /// A family with no faces cannot appear here by construction, which is the property that
    /// makes the picker honest on a machine this was never run on.
    pub fn families(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .fonts
            .db()
            .faces()
            .flat_map(|face| face.families.iter().map(|(name, _)| name.clone()))
            .collect();
        names.sort_unstable();
        names.dedup();
        names
    }

    /// Number of faces available to shaping, counting every style of every family.
    pub fn face_count(&self) -> usize {
        self.fonts.db().len()
    }

    /// Drops every rasterised glyph bitmap, keeping the fonts and the shaping.
    ///
    /// The bitmap caches are keyed by [`GlyphKey`], which folds in the *device* size —
    /// the logical size already multiplied by the zoom. So the instant the camera's
    /// zoom moves, every bitmap in them is unreachable: no future key can match one.
    /// Neither cache has an eviction policy of its own, so without this a zoom gesture
    /// leaves a fresh bitmap set resident per frame for the life of the process.
    ///
    /// Free to call: it only discards entries that could never be read again. Callers
    /// should call it when the scale they rasterise at changes, not every frame —
    /// while the zoom is still, the cache is doing its job.
    pub fn forget_glyph_bitmaps(&mut self) {
        self.glyphs.image_cache.clear();
        self.glyphs.outline_command_cache.clear();
    }

    /// How many glyph bitmaps are resident. For tests and the memory HUD.
    pub fn glyph_bitmaps(&self) -> usize {
        self.glyphs.image_cache.len()
    }

    /// Whether any glyph bitmap is cached for a device size other than `scale` would
    /// produce. Test-only: the leak this guards against is invisible from outside.
    #[cfg(test)]
    pub(crate) fn glyph_bitmap_sizes(&self) -> std::collections::BTreeSet<u32> {
        self.glyphs
            .image_cache
            .keys()
            .map(|key| key.font_size_bits)
            .collect()
    }

    /// Shapes and positions `text`.
    pub fn layout(&mut self, text: &StyledText, params: &LayoutParams) -> Layout {
        let buffer = self.shape(text, params);
        let mut lines = Vec::new();
        let mut extent = TextExtent { width: 0.0, height: 0.0, lines: 0 };

        for run in buffer.layout_runs() {
            extent.width = extent.width.max(run.line_w);
            extent.height = run.line_top + run.line_height;
            extent.lines += 1;
            lines.push(LaidOutLine {
                paragraph: run.line_i,
                top: run.line_top,
                baseline: run.line_y,
                height: run.line_height,
                width: run.line_w,
                glyphs: run
                    .glyphs
                    .iter()
                    .map(|glyph| PlacedGlyph {
                        x: glyph.x,
                        // cosmic-text reports `y` relative to the baseline and the
                        // baseline separately; folding them makes every glyph
                        // block-relative, which is the only frame the renderer and
                        // the hit-tester both want.
                        y: run.line_y + glyph.y,
                        advance: glyph.w,
                        font_size: glyph.font_size,
                        x_offset: glyph.x_offset,
                        y_offset: glyph.y_offset,
                        font: glyph.font_id,
                        glyph_id: glyph.glyph_id,
                        color: glyph.color_opt.map(|c| Rgb::new(c.r(), c.g(), c.b())),
                        span: (glyph.metadata != NO_SPAN).then_some(glyph.metadata),
                        cluster: glyph.start..glyph.end,
                        flags: glyph.cache_key_flags,
                    })
                    .collect(),
            });
        }

        Layout { lines, extent }
    }

    /// The bounding box `text` would occupy, without rasterising anything.
    ///
    /// Shaping still happens — there is no way to know a line's width without it —
    /// but no glyph is scaled, no bitmap is produced and no `Vec` per line is
    /// allocated. Auto-fit calls this about a dozen times per sticky, so the
    /// difference is the difference between an import that stalls and one that
    /// does not.
    pub fn measure(&mut self, text: &StyledText, params: &LayoutParams) -> TextExtent {
        let buffer = self.shape(text, params);
        let mut extent = TextExtent { width: 0.0, height: 0.0, lines: 0 };
        for run in buffer.layout_runs() {
            extent.width = extent.width.max(run.line_w);
            extent.height = run.line_top + run.line_height;
            extent.lines += 1;
        }
        extent
    }

    /// Whether a bold face exists in the family a request would resolve to.
    ///
    /// **Asking cosmic-text for a weight the family has not got does not fall back to
    /// that family's regular face — it falls back to another family entirely, and on
    /// this machine that is a monospace one.** Measured before the guard existed: at
    /// 18px, `"WWWWWWWW"` and `"IIIIIIII"` came out to an advance ratio of 4.11 in
    /// regular Noto Sans and **1.00** in "bold" — every glyph the same width, because
    /// the shaper had silently moved to Courier. A bold sticky rendered as typewriter
    /// text, and the box measured around it was wrong by the same amount.
    ///
    /// So a bold span whose family has no bold face is shaped **regular in the right
    /// family** instead. That loses the emphasis, which is a visible degradation and
    /// deliberately the lesser one: the reader still sees the words in the typeface the
    /// rest of the board is set in. cosmic-text has no synthetic bold to fall back on,
    /// so there is no third option.
    ///
    /// Weight 600 is the bar rather than 700, so a family shipping only SemiBold still
    /// counts — the request is for emphasis, not for a number.
    ///
    /// The existing test for this path only asserted `bold.width >= regular.width`,
    /// which a monospace fallback satisfies comfortably; that is why it was green.
    fn family_has_bold(&mut self, family: Option<&str>) -> bool {
        let db = self.fonts.db();
        // `None` means "whatever sans-serif resolves to", which is a real family name
        // that has to be looked up before its faces can be counted.
        let resolved = match family {
            Some(name) => name.to_owned(),
            None => db.family_name(&Family::SansSerif).to_owned(),
        };
        db.faces().any(|face| {
            face.weight.0 >= 600
                && face.families.iter().any(|(name, _)| name.eq_ignore_ascii_case(&resolved))
        })
    }

    fn shape(&mut self, text: &StyledText, params: &LayoutParams) -> Buffer {
        let mut buffer = Buffer::new_empty(params.metrics());
        buffer.set_wrap(
            &mut self.fonts,
            // A word wider than the box must still break, or the extent overflows
            // and auto-fit can never converge.
            if params.wrap_width().is_some() { Wrap::WordOrGlyph } else { Wrap::None },
        );
        // The height stays `None` on purpose — see the module docs. Clipping is the
        // caller's job; a buffer that clips its own layout cannot be measured.
        buffer.set_size(&mut self.fonts, params.wrap_width(), None);

        let defaults = base_attrs(params);
        // Asking for a weight the family has not got is worse than not asking. See
        // [`Self::family_has_bold`].
        let bold = self.family_has_bold(params.font_family.as_deref());
        let mut spans: Vec<_> = text
            .spans()
            .iter()
            .enumerate()
            .map(|(index, span)| {
                (span.text.as_str(), span_attrs(&defaults, index, &span.style, bold))
            })
            .collect();

        // cosmic-text treats a trailing paragraph separator as a line *terminator*:
        // both `BidiParagraphs` (used by `set_rich_text`) and `LineIter` (used by
        // `set_text`) yield one line for `"fan\n"`. Miro's editor means it as a
        // real, empty last line — `<p>fan</p><p><br /></p>` is a sticky with a blank
        // second line, and swallowing it makes the note auto-fit a size too large.
        // One more separator materialises the line; it sits past every span's byte
        // range, so no cluster offset moves.
        if text.spans().last().is_some_and(|span| span.text.ends_with('\n')) {
            spans.push(("\n", defaults.clone()));
        }

        buffer.set_rich_text(
            &mut self.fonts,
            spans,
            &defaults,
            Shaping::Advanced,
            Some(params.align.to_cosmic()),
        );
        buffer
    }
}

/// Metadata value on the default attributes.
///
/// `Buffer::set_rich_text` records a span only when its attributes differ from the
/// defaults, so the defaults must carry a value no real span index can take —
/// otherwise a span whose style happens to match the defaults would lose its
/// identity and, with it, its underline and its link.
const NO_SPAN: usize = usize::MAX;

fn base_attrs(params: &LayoutParams) -> Attrs<'_> {
    let attrs = Attrs::new().metadata(NO_SPAN);
    match &params.font_family {
        Some(name) => attrs.family(Family::Name(name)),
        None => attrs,
    }
}

fn span_attrs<'a>(
    defaults: &Attrs<'a>,
    index: usize,
    style: &SpanStyle,
    bold_available: bool,
) -> Attrs<'a> {
    let mut attrs = defaults.clone().metadata(index);
    if style.bold && bold_available {
        attrs = attrs.weight(Weight::BOLD);
    }
    if style.italic {
        attrs = attrs.style(Style::Italic);
    }
    if let Some(rgb) = style.color {
        attrs = attrs.color(Color::rgb(rgb.r, rgb.g, rgb.b));
    }
    // Underline, strike and link are carried by `metadata` alone: cosmic-text has
    // no model for them, and the span index is enough to recover all three.
    attrs
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::span::TextSpan;
    use std::sync::{Mutex, MutexGuard, OnceLock};

    /// One engine for the whole suite.
    ///
    /// `FontSystem::new` enumerates every installed font, which costs more than the
    /// assertions do. Sharing it also means a test that corrupts the caches would
    /// be visible to the others, which is the behaviour we want to catch.
    pub(crate) fn engine() -> MutexGuard<'static, TextEngine> {
        static ENGINE: OnceLock<Mutex<TextEngine>> = OnceLock::new();
        ENGINE
            .get_or_init(|| {
                Mutex::new(TextEngine::new().expect(
                    "the test suite shapes with real system fonts; \
                     macOS and Windows always have some",
                ))
            })
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn params(font_size: f32) -> LayoutParams {
        LayoutParams { font_size, ..LayoutParams::default() }
    }

    #[test]
    fn an_engine_reports_the_faces_it_can_shape_with() {
        assert!(engine().face_count() > 0);
    }

    // ----- caret geometry -----------------------------------------------------------

    #[test]
    fn paragraph_starts_are_the_offsets_after_each_separator() {
        assert_eq!(paragraph_starts(""), [0]);
        assert_eq!(paragraph_starts("one"), [0]);
        assert_eq!(paragraph_starts("one\ntwo"), [0, 4]);
        // A trailing separator opens a real, empty last paragraph — Miro's editor means
        // `<p>fan</p><p><br /></p>` as a blank second line, and a caret has to be able to
        // sit in it.
        assert_eq!(paragraph_starts("one\n"), [0, 4]);
    }

    /// The caret walks left to right across a single line, monotonically, and ends up
    /// past the last glyph rather than back at the left edge.
    #[test]
    fn the_caret_advances_through_a_line_and_stops_after_the_last_glyph() {
        let mut engine = engine();
        let text = "abcdef";
        let layout = engine.layout(&StyledText::plain(text), &params(20.0));

        let xs: Vec<f32> = (0..=text.len()).map(|b| layout.caret(text, b).x).collect();
        assert!(xs.windows(2).all(|w| w[1] > w[0]), "caret x was not monotonic: {xs:?}");
        assert!((xs[0] - 0.0).abs() < 0.01, "the caret at offset 0 was at {}", xs[0]);
        assert!(
            (xs[text.len()] - layout.extent.width).abs() < 0.5,
            "the caret at the end was at {} for a block {} wide",
            xs[text.len()],
            layout.extent.width,
        );
        // Every caret on a one-line block shares that line's box.
        assert!((0..=text.len()).all(|b| layout.caret(text, b).line == 0));
    }

    /// An offset past the end clamps rather than panicking or returning a wild x, and an
    /// empty block still has a caret with real height — there is nowhere else to put it.
    #[test]
    fn an_out_of_range_offset_clamps_and_an_empty_block_still_has_a_caret() {
        let mut engine = engine();
        let text = "hi";
        let layout = engine.layout(&StyledText::plain(text), &params(20.0));
        assert_eq!(layout.caret(text, 999), layout.caret(text, text.len()));

        let empty = engine.layout(&StyledText::plain(""), &params(20.0));
        let caret = empty.caret("", 0);
        assert!(caret.height > 0.0, "an empty block reserved no caret height");
        assert!((caret.x - 0.0).abs() < 0.01);
    }

    /// The reason [`LaidOutLine::paragraph`] had to be recorded: a glyph's cluster range
    /// is per-paragraph, so without it every offset on line two resolves as though it
    /// were on line one.
    #[test]
    fn the_caret_finds_the_right_paragraph_across_a_newline() {
        let mut engine = engine();
        let text = "one\ntwo";
        let layout = engine.layout(&StyledText::plain(text), &params(20.0));
        assert_eq!(layout.lines.len(), 2, "two paragraphs are two lines");

        // Offset 0 is the start of "one"; offset 4 is the start of "two". Both are at the
        // left edge, but on different lines — which is exactly what a per-paragraph
        // cluster range cannot express on its own.
        assert_eq!(layout.caret(text, 0).line, 0);
        assert_eq!(layout.caret(text, 4).line, 1);
        assert!(layout.caret(text, 4).top > layout.caret(text, 0).top);
        // And the end of the first paragraph is still on the first line.
        assert_eq!(layout.caret(text, 3).line, 0);
        assert!(layout.caret(text, 3).x > 0.0);
    }

    /// A caret at a soft wrap belongs to the *following* line — where the next character
    /// will appear, which is the convention every editor follows.
    #[test]
    fn a_caret_at_a_soft_wrap_sits_on_the_following_line() {
        let mut engine = engine();
        let text = "aaaa bbbb cccc dddd eeee ffff";
        let narrow = LayoutParams { max_width: Some(60.0), ..params(20.0) };
        let layout = engine.layout(&StyledText::plain(text), &narrow);
        assert!(layout.lines.len() > 2, "the fixture did not wrap: {} lines", layout.lines.len());
        // One paragraph, several visual lines.
        assert!(layout.lines.iter().all(|line| line.paragraph == 0));

        // Sweeping the whole string never moves the caret upwards.
        let tops: Vec<f32> = (0..=text.len()).map(|b| layout.caret(text, b).top).collect();
        assert!(tops.windows(2).all(|w| w[1] >= w[0]), "the caret jumped back up: {tops:?}");
        assert!(tops[text.len()] > tops[0], "every offset landed on one line");
    }

    /// Clicking round-trips: the offset a point resolves to puts the caret back at
    /// (about) that point. This is the property that makes a click land where the user
    /// aimed, and it is checked as a round trip because the two functions are each
    /// other's inverse and a sign error in either would pass a one-sided test.
    #[test]
    fn clicking_and_the_caret_are_inverses() {
        let mut engine = engine();
        let text = "the quick brown fox";
        let layout = engine.layout(&StyledText::plain(text), &params(20.0));
        let line = &layout.lines[0];
        let middle = line.top + line.height * 0.5;

        for byte in 0..=text.len() {
            if !text.is_char_boundary(byte) {
                continue;
            }
            let caret = layout.caret(text, byte);
            // Nudge inside the glyph that follows, so the "nearer edge" rule resolves
            // back to this offset rather than to its neighbour.
            let back = layout.byte_at(text, caret.x + 0.5, middle);
            assert_eq!(back, byte, "offset {byte} came back as {back}");
        }
    }

    /// A click outside the block clamps to its ends rather than returning nothing —
    /// which is what dragging a selection off the top of a paragraph needs.
    #[test]
    fn a_click_outside_the_block_clamps_to_its_ends() {
        let mut engine = engine();
        let text = "one\ntwo";
        let layout = engine.layout(&StyledText::plain(text), &params(20.0));
        assert_eq!(layout.byte_at(text, -100.0, -100.0), 0);
        assert_eq!(layout.byte_at(text, 9_999.0, 9_999.0), text.len());
        // Far right of the *first* line is the end of that paragraph, not of the text.
        let first = &layout.lines[0];
        assert_eq!(layout.byte_at(text, 9_999.0, first.top + 1.0), 3);
    }

    /// A selection covers the glyphs in its range and nothing else, one box per visual
    /// line, and an empty range draws nothing — a zero-width highlight is a caret drawn
    /// twice.
    #[test]
    fn a_selection_is_one_box_per_line_and_nothing_for_an_empty_range() {
        let mut engine = engine();
        let text = "one\ntwo";
        let layout = engine.layout(&StyledText::plain(text), &params(20.0));

        assert!(layout.selection_boxes(text, 2..2).is_empty());
        // Reversed ranges are the same selection: a drag can end left of where it began.
        // Built from variables because a `7..0` literal is a clippy error in its own
        // right, and the point here is that this function tolerates one.
        let (low, high) = (0, 7);
        assert_eq!(
            layout.selection_boxes(text, low..high),
            layout.selection_boxes(text, high..low),
        );

        let all = layout.selection_boxes(text, 0..text.len());
        assert_eq!(all.len(), 2, "two lines should give two boxes");
        assert!(all.iter().all(|b| b.width > 0.0 && b.height > 0.0));
        assert_eq!(all[0].line, 0);
        assert_eq!(all[1].line, 1);

        // A selection inside one line touches only that line, and is narrower than the
        // whole line.
        let partial = layout.selection_boxes(text, 0..2);
        assert_eq!(partial.len(), 1);
        assert!(partial[0].width < all[0].width, "{} vs {}", partial[0].width, all[0].width);
    }

    /// A selection ending at a newline covers the first paragraph and does not spill a
    /// box onto the second. The separator is not a glyph, so a naive "any line whose
    /// paragraph is in range" test would draw one anyway.
    #[test]
    fn a_selection_ending_at_a_newline_does_not_spill_onto_the_next_line() {
        let mut engine = engine();
        let text = "one\ntwo";
        let layout = engine.layout(&StyledText::plain(text), &params(20.0));
        let boxes = layout.selection_boxes(text, 0..4);
        assert_eq!(boxes.len(), 1, "{boxes:?}");
        assert_eq!(boxes[0].line, 0);
    }

    /// The clamp exists because `Buffer::new_empty` asserts on a zero line height.
    /// A widget carrying Miro's `fs: 0` auto-fit marker must not reach layout, but
    /// if it does it must not take the process with it.
    #[test]
    fn degenerate_sizes_are_clamped_rather_than_panicking() {
        for size in [0.0, -12.0, f32::NAN, f32::INFINITY] {
            let p = params(size);
            assert!(p.effective_font_size() >= MIN_FONT_SIZE, "size {size}");
            assert!(p.effective_line_height() > 0.0, "size {size}");
            let extent = engine().measure(&StyledText::plain("x"), &p);
            assert!(extent.height > 0.0, "size {size}");
        }
    }

    #[test]
    fn a_line_height_multiplier_scales_the_measured_height() {
        let text = StyledText::plain("one\ntwo");
        let mut engine = engine();
        let single = engine.measure(&text, &LayoutParams { line_height: 1.0, ..params(20.0) });
        let wide = engine.measure(&text, &LayoutParams { line_height: 2.0, ..params(20.0) });
        assert_eq!(single.lines, 2);
        assert_eq!(wide.lines, 2);
        assert!((wide.height - single.height * 2.0).abs() < 0.01, "{single:?} {wide:?}");
    }

    /// Auto-fit's whole search rests on this being monotone.
    #[test]
    fn a_larger_font_measures_larger() {
        let text = StyledText::plain("The quick brown fox");
        let mut engine = engine();
        let small = engine.measure(&text, &params(12.0));
        let large = engine.measure(&text, &params(24.0));
        assert!(large.width > small.width, "{small:?} {large:?}");
        assert!(large.height > small.height, "{small:?} {large:?}");
    }

    #[test]
    fn empty_text_still_occupies_one_line() {
        let extent = engine().measure(&StyledText::default(), &params(20.0));
        assert_eq!(extent.lines, 1);
        assert_eq!(extent.width, 0.0);
        assert!(extent.height > 0.0, "an empty sticky must still hold a caret");
    }

    #[test]
    fn hard_breaks_become_separate_lines() {
        let extent = engine().measure(&StyledText::plain("a\nb\nc"), &params(16.0));
        assert_eq!(extent.lines, 3);
    }

    /// The verified sticky body ends in a hard break, and that blank last line is
    /// what makes the sticky the height it is in Miro.
    #[test]
    fn the_verified_sticky_text_measures_two_lines() {
        let text = crate::from_miro_html("<p>fan</p><p><br /></p>");
        let extent = engine().measure(&text, &params(24.0));
        assert_eq!(extent.lines, 2);
    }

    #[test]
    fn wrapping_at_a_narrow_width_adds_lines_and_bounds_the_width() {
        let text = StyledText::plain("The quick brown fox jumps over the lazy dog");
        let mut engine = engine();
        let unwrapped = engine.measure(&text, &params(16.0));
        let wrapped = engine.measure(&text, &params(16.0).with_max_width(Some(120.0)));
        assert_eq!(unwrapped.lines, 1);
        assert!(wrapped.lines > 1, "{wrapped:?}");
        assert!(wrapped.width <= 120.0 + 0.5, "{wrapped:?}");
        assert!(wrapped.height > unwrapped.height);
    }

    /// `Wrap::WordOrGlyph` rather than `Wrap::Word`: a single unbreakable token
    /// wider than the box must still break, or the extent never fits and auto-fit
    /// runs to its lower bound.
    #[test]
    fn a_word_wider_than_the_box_is_broken_rather_than_overflowing() {
        let text = StyledText::plain("Kraftfahrzeughaftpflichtversicherung");
        let extent = engine().measure(&text, &params(24.0).with_max_width(Some(60.0)));
        assert!(extent.lines > 1, "{extent:?}");
    }

    #[test]
    fn a_zero_or_negative_wrap_width_means_no_wrapping() {
        let text = StyledText::plain("The quick brown fox jumps over the lazy dog");
        let mut engine = engine();
        let natural = engine.measure(&text, &params(16.0));
        for width in [0.0, -10.0, f32::NAN] {
            let extent = engine.measure(&text, &params(16.0).with_max_width(Some(width)));
            assert_eq!(extent.lines, natural.lines, "width {width}");
        }
    }

    #[test]
    fn layout_and_measure_agree() {
        let text = StyledText::plain("The quick brown fox");
        let mut engine = engine();
        let p = params(18.0).with_max_width(Some(90.0));
        let laid = engine.layout(&text, &p);
        let measured = engine.measure(&text, &p);
        assert_eq!(laid.extent, measured);
        assert_eq!(laid.lines.len(), measured.lines);
    }

    #[test]
    fn glyphs_carry_block_relative_positions_in_reading_order() {
        let laid = engine().layout(&StyledText::plain("abc"), &params(32.0));
        let glyphs: Vec<_> = laid.glyphs().collect();
        assert_eq!(glyphs.len(), 3);
        assert!(glyphs[0].x < glyphs[1].x && glyphs[1].x < glyphs[2].x);
        // The baseline sits below the top of the block, never at it.
        assert!(glyphs.iter().all(|g| g.y > 0.0), "{glyphs:?}");
        assert!(glyphs.iter().all(|g| g.advance > 0.0));
        assert_eq!(laid.lines[0].glyphs[0].cluster, 0..1);
    }

    /// The span index is the only channel underline, strike and link have, so it
    /// must survive shaping — including for a span whose attributes happen to match
    /// the defaults exactly.
    #[test]
    fn every_glyph_carries_the_index_of_its_span() {
        let text = StyledText::from_spans([
            TextSpan::plain("aa"),
            TextSpan::new("bb", SpanStyle { underline: true, ..SpanStyle::default() }),
        ]);
        let laid = engine().layout(&text, &params(20.0));
        let spans: Vec<_> = laid.glyphs().map(|g| g.span).collect();
        assert_eq!(spans, [Some(0), Some(0), Some(1), Some(1)]);
    }

    /// The separator appended to materialise a trailing blank line must not shift
    /// any real span's identity, or every underline after it would move.
    #[test]
    fn the_appended_trailing_separator_does_not_disturb_span_indices() {
        let text = StyledText::from_spans([
            TextSpan::plain("a"),
            TextSpan::new("b", SpanStyle::bold()),
            TextSpan::plain("\n"),
        ]);
        assert!(text.to_plain().ends_with('\n'));
        let laid = engine().layout(&text, &params(20.0));
        assert_eq!(laid.extent.lines, 2);
        let spans: Vec<_> = laid.glyphs().map(|g| g.span).collect();
        assert_eq!(spans, [Some(0), Some(1)], "the blank line contributes no glyph");
    }

    #[test]
    fn a_span_colour_reaches_the_glyph() {
        let text = StyledText::from_spans([
            TextSpan::plain("a"),
            TextSpan::new(
                "b",
                SpanStyle { color: Some(Rgb::new(0xFF, 0xF7, 0x9E)), ..SpanStyle::default() },
            ),
        ]);
        let laid = engine().layout(&text, &params(20.0));
        let colors: Vec<_> = laid.glyphs().map(|g| g.color).collect();
        assert_eq!(colors, [None, Some(Rgb::new(0xFF, 0xF7, 0x9E))]);
    }

    #[test]
    fn bold_and_italic_reach_shaping() {
        let text = StyledText::plain("nnnnnnnn");
        let mut engine = engine();
        let regular = engine.measure(&text, &params(40.0));
        let bold = engine.measure(
            &StyledText::from_spans([TextSpan::new("nnnnnnnn", SpanStyle::bold())]),
            &params(40.0),
        );
        // Bold is wider in every family that has a bold face, and where one does not
        // exist `family_has_bold` drops the request rather than letting the shaper leave
        // the family — so equal is the correct answer there.
        assert!(bold.width >= regular.width, "{regular:?} {bold:?}");
    }

    /// The default family must actually *have* a bold, and asking for it must make the text
    /// heavier.
    ///
    /// # Why this is measured on ink coverage rather than on width
    ///
    /// `a_bold_span_never_escapes_its_family` below already proves bold does not *change
    /// typeface*, and it passed throughout the years the app shipped no bold at all — a
    /// request that is silently dropped keeps the family perfectly. Width cannot separate the
    /// two either: Inter's Bold is only a few percent wider than its Regular, well inside the
    /// noise of any threshold loose enough not to be brittle.
    ///
    /// So this asserts the property that only a real weight change produces: `family_has_bold`
    /// answers **true** for the default family. That is the single condition the whole bundle
    /// exists to satisfy, and it was `false` on this machine before it — `sans-serif` resolved
    /// to Noto Sans, which ships regular only.
    #[test]
    fn the_default_family_ships_a_real_bold() {
        let mut engine = TextEngine::new().expect("the test machine has fonts");
        assert!(
            engine.family_has_bold(None),
            "the default family has no bold face, so every bold span in the app is silently \
             shaping at regular weight — see BUNDLED_FONTS"
        );
        assert!(
            engine.family_has_bold(Some(BUNDLED_FAMILY)),
            "the bundled family is not in the database under the name the alias points at"
        );

        // …and the permission is *used*. `family_has_bold` only says the face exists; a span
        // that asks for bold and shapes at regular anyway is the bug this whole change is
        // about, and it looks identical from here unless the metrics are compared. Measured
        // on Inter: 535.16 regular against 550.51 bold, a 2.87% delta. The bar is 1% — enough
        // to be impossible without a real weight change, loose enough to survive a font
        // update — and it is **0.0%** when the request is dropped.
        let params = LayoutParams { font_size: 40.0, ..LayoutParams::default() };
        let run = |bold: bool| {
            StyledText::from_spans([TextSpan::new(
                "Finally Driving The Acme M5",
                SpanStyle { bold, ..SpanStyle::default() },
            )])
        };
        let (regular, heavy) =
            (engine.measure(&run(false), &params).width, engine.measure(&run(true), &params).width);
        assert!(
            f64::from(heavy) > f64::from(regular) * 1.01,
            "asking for bold changed nothing: {regular:.2} against {heavy:.2} — the weight is \
             being dropped at shaping"
        );
    }

    /// The bug this test exists for: **a bold span must never change typeface.**
    ///
    /// Asking cosmic-text for a weight the family has not got made it fall back to a
    /// different family, and on this machine that was a monospace one — so a bold
    /// sticky rendered as typewriter text and its measured box was wrong to match.
    /// See [`TextEngine::family_has_bold`].
    ///
    /// Checked by *proportionality* rather than by width, because that is the property
    /// that separates the two families rather than the two weights: the ratio of a run
    /// of `W` to a run of `I` is about 4:1 in any proportional face and exactly 1:1 in a
    /// monospace one. A width assertion cannot tell "bold" from "Courier" — which is
    /// precisely why the test above was green through the whole bug.
    #[test]
    fn a_bold_span_never_escapes_its_family() {
        let mut engine = engine();
        // Every family in `docs/05-design-language.md` §5, plus the default. Noto Sans
        // is the one that actually reproduced it: installed for Miro import fidelity,
        // regular weight only.
        for family in [None, Some("Noto Sans"), Some("Inter"), Some("Helvetica")] {
            let mut ratio = |bold: bool| {
                let params = LayoutParams {
                    font_family: family.map(str::to_owned),
                    ..params(18.0)
                };
                let run = |s: &str| {
                    StyledText::from_spans([TextSpan::new(
                        s,
                        SpanStyle { bold, ..SpanStyle::default() },
                    )])
                };
                let narrow = engine.measure(&run("IIIIIIII"), &params).width;
                let wide = engine.measure(&run("WWWWWWWW"), &params).width;
                assert!(narrow > 0.0, "{family:?} measured nothing");
                wide / narrow
            };
            let (regular, bold) = (ratio(false), ratio(true));
            assert!(
                bold > 2.0,
                "{family:?} shaped bold in a monospace face: W/I was {bold:.2}",
            );
            assert!(
                (bold - regular).abs() < 0.5,
                "{family:?} changed proportions from {regular:.2} to {bold:.2} — that is a \
                 different typeface, not a heavier one",
            );
        }
    }

    #[test]
    fn alignment_shifts_a_short_line_inside_the_wrap_width() {
        let text = StyledText::plain("x");
        let mut engine = engine();
        let width = Some(200.0);
        let left = engine.layout(&text, &LayoutParams { align: TextAlign::Left, ..params(20.0) }.with_max_width(width));
        let centre = engine.layout(&text, &LayoutParams { align: TextAlign::Center, ..params(20.0) }.with_max_width(width));
        let right = engine.layout(&text, &LayoutParams { align: TextAlign::Right, ..params(20.0) }.with_max_width(width));
        let x = |l: &Layout| l.lines[0].glyphs[0].x;
        assert!(x(&left) < x(&centre), "{} {}", x(&left), x(&centre));
        assert!(x(&centre) < x(&right), "{} {}", x(&centre), x(&right));
    }

    #[test]
    fn decoration_runs_cover_only_the_decorated_spans() {
        let text = StyledText::from_spans([
            TextSpan::plain("no "),
            TextSpan::new("yes", SpanStyle { underline: true, ..SpanStyle::default() }),
            TextSpan::plain(" no"),
        ]);
        let laid = engine().layout(&text, &params(24.0));
        let runs = laid.decoration_runs(&text);
        assert_eq!(runs.len(), 1, "{runs:?}");
        assert_eq!(runs[0].decoration, Decoration::Underline);
        assert_eq!(runs[0].line, 0);
        assert!(runs[0].width > 0.0);
        assert_eq!(runs[0].baseline, laid.lines[0].baseline);

        // The run must start at the first decorated glyph, not at the line.
        let first_underlined = laid.glyphs().find(|g| g.span == Some(1)).unwrap();
        assert!((runs[0].x - first_underlined.x).abs() < 0.01);
    }

    #[test]
    fn underline_and_strikethrough_are_separate_runs() {
        let text = StyledText::from_spans([TextSpan::new(
            "both",
            SpanStyle { underline: true, strikethrough: true, ..SpanStyle::default() },
        )]);
        let laid = engine().layout(&text, &params(24.0));
        let runs = laid.decoration_runs(&text);
        assert_eq!(runs.len(), 2);
        assert!(runs.iter().any(|r| r.decoration == Decoration::Underline));
        assert!(runs.iter().any(|r| r.decoration == Decoration::Strikethrough));
    }

    /// A decoration that changes colour mid-run must be two rules, not one rule in
    /// whichever colour happened to come first.
    #[test]
    fn a_colour_change_splits_a_decoration_run() {
        let underline = |color| SpanStyle { underline: true, color, ..SpanStyle::default() };
        let text = StyledText::from_spans([
            TextSpan::new("aa", underline(Some(Rgb::new(0xFF, 0, 0)))),
            TextSpan::new("bb", underline(Some(Rgb::new(0, 0, 0xFF)))),
        ]);
        let laid = engine().layout(&text, &params(24.0));
        let runs = laid.decoration_runs(&text);
        assert_eq!(runs.len(), 2, "{runs:?}");
        assert_eq!(runs[0].color, Some(Rgb::new(0xFF, 0, 0)));
        assert_eq!(runs[1].color, Some(Rgb::new(0, 0, 0xFF)));
    }

    #[test]
    fn text_with_no_decorations_produces_no_runs() {
        let text = StyledText::plain("nothing here");
        let laid = engine().layout(&text, &params(20.0));
        assert!(laid.decoration_runs(&text).is_empty());
    }

    #[test]
    fn physical_placement_snaps_to_the_pixel_grid_and_keys_by_scale() {
        let laid = engine().layout(&StyledText::plain("a"), &params(20.0));
        let glyph = &laid.lines[0].glyphs[0];

        let at_1x = glyph.physical((0.0, 0.0), 1.0);
        let at_2x = glyph.physical((0.0, 0.0), 2.0);
        assert!((at_1x.key.font_size() - 20.0).abs() < 0.001);
        assert!((at_2x.key.font_size() - 40.0).abs() < 0.001);
        assert_ne!(at_1x.key, at_2x.key, "an atlas must not share a bitmap across zooms");
        assert_eq!(at_1x.key.glyph_id(), at_2x.key.glyph_id());

        // Vertical hinting: the y phase is always zero, so a glyph costs at most
        // four atlas entries per size rather than sixteen.
        assert_eq!(at_1x.key.subpixel_offset().1, 0.0);

        // Translating by a whole pixel must reuse the same bitmap.
        let shifted = glyph.physical((3.0, 5.0), 1.0);
        assert_eq!(shifted.key, at_1x.key);
        assert_eq!(shifted.x, at_1x.x + 3);
        assert_eq!(shifted.y, at_1x.y + 5);
    }

    /// A subpixel shift must change the key, or text at fractional positions would
    /// be drawn from a bitmap rendered for a different phase and look uneven.
    #[test]
    fn a_horizontal_subpixel_shift_changes_the_key() {
        let laid = engine().layout(&StyledText::plain("a"), &params(20.0));
        let glyph = &laid.lines[0].glyphs[0];
        let aligned = glyph.physical((0.0, 0.0), 1.0);
        let half = glyph.physical((0.5, 0.0), 1.0);
        assert_ne!(aligned.key, half.key);
        assert_eq!(half.key.subpixel_offset().0, 0.5);
    }
}

