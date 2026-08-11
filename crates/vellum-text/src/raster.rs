//! Glyph rasterisation, in the form a GPU atlas packer consumes.
//!
//! This module produces **data, never GPU calls**: a coverage bitmap plus the
//! offsets needed to place it, keyed by [`GlyphKey`]. Uploading, packing and
//! eviction belong to `vellum-render`, and keeping the boundary here means the
//! rasteriser is testable without a device and reusable by anything that needs a
//! glyph image — a thumbnail, an SVG export, a headless golden-image test.
//!
//! Two properties of the output matter to a packer:
//!
//! - **A key identifies a bitmap completely.** Face, glyph, device-pixel size and
//!   subpixel phase are all in it, so two glyphs with equal keys are the same image
//!   and may share one atlas slot. Nothing else needs to be compared.
//! - **Placement is separate from coverage.** `left`/`top` are the bitmap's offset
//!   from the pen position, so the atlas stores tight bitmaps and the vertex
//!   generator adds the bearing. Baking the bearing into the bitmap would waste
//!   most of the atlas on transparent pixels.
//!
//! Colour glyphs (emoji) come back as RGBA rather than coverage. A packer must
//! branch on [`GlyphImage::content`]: the two cannot share an atlas, because one is
//! tinted by the shader and the other must not be.

use crate::layout::{GlyphKey, Layout, TextEngine};
use cosmic_text::SwashContent;
use std::collections::HashMap;

/// What the bytes in a [`GlyphImage`] mean.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GlyphContent {
    /// One byte per pixel: alpha coverage, to be tinted with the run's colour.
    Coverage,
    /// Four bytes per pixel: independent R, G and B coverage for LCD subpixel
    /// rendering. Not requested by this crate — cosmic-text asks swash for
    /// `Format::Alpha` — but a face can still return it, so it is represented
    /// rather than silently mistaken for colour.
    SubpixelCoverage,
    /// Four bytes per pixel, RGBA: a colour bitmap or colour outline, drawn as-is.
    Color,
}

impl GlyphContent {
    pub fn bytes_per_pixel(self) -> usize {
        match self {
            Self::Coverage => 1,
            Self::SubpixelCoverage | Self::Color => 4,
        }
    }

    fn from_swash(content: SwashContent) -> Self {
        match content {
            SwashContent::Mask => Self::Coverage,
            SwashContent::SubpixelMask => Self::SubpixelCoverage,
            SwashContent::Color => Self::Color,
        }
    }
}

/// One rasterised glyph: a tight bitmap plus where to put it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GlyphImage {
    pub content: GlyphContent,
    pub width: u32,
    pub height: u32,
    /// Offset from the pen position to the bitmap's left edge, in device px.
    /// Usually small and positive; negative for glyphs that lean left of their
    /// origin, which is why it is signed.
    pub left: i32,
    /// Offset from the baseline **up** to the bitmap's top edge. The row at
    /// `top` is above the baseline, so a renderer subtracts it.
    pub top: i32,
    /// Row-major, no padding: `width * height * content.bytes_per_pixel()` bytes.
    pub data: Vec<u8>,
}

impl GlyphImage {
    /// True for a glyph with no ink — a space, or a mark that rendered empty.
    /// A packer should skip these rather than allocate a zero-area slot.
    pub fn is_blank(&self) -> bool {
        self.width == 0 || self.height == 0
    }

    /// Bytes per row, which is what an upload's stride is computed from.
    pub fn row_bytes(&self) -> usize {
        self.width as usize * self.content.bytes_per_pixel()
    }

    /// Coverage at `(x, y)`, or `None` outside the bitmap.
    ///
    /// For a colour glyph this is the alpha channel. Exists so tests can assert on
    /// ink without a GPU; it is not how a renderer should read pixels.
    pub fn coverage_at(&self, x: u32, y: u32) -> Option<u8> {
        if x >= self.width || y >= self.height {
            return None;
        }
        let pixel = (y as usize * self.width as usize + x as usize)
            * self.content.bytes_per_pixel();
        match self.content {
            GlyphContent::Coverage => self.data.get(pixel).copied(),
            // RGBA: alpha is the fourth byte.
            GlyphContent::SubpixelCoverage | GlyphContent::Color => {
                self.data.get(pixel + 3).copied()
            }
        }
    }
}

impl TextEngine {
    /// Rasterises one glyph.
    ///
    /// `None` means the face could not produce an image at all — a missing font or
    /// a glyph id the face does not have. A glyph that is legitimately blank comes
    /// back as `Some` with zero dimensions, so a caller can tell "no ink" from
    /// "could not render".
    ///
    /// Results are cached inside the engine, keyed by exactly the key passed in;
    /// re-rasterising the same glyph across frames is a hash lookup.
    pub fn rasterise(&mut self, key: GlyphKey) -> Option<GlyphImage> {
        let image = self.glyphs.get_image(&mut self.fonts, key.0).as_ref()?;
        Some(GlyphImage {
            content: GlyphContent::from_swash(image.content),
            width: image.placement.width,
            height: image.placement.height,
            left: image.placement.left,
            top: image.placement.top,
            data: image.data.clone(),
        })
    }

    /// Every distinct glyph image `layout` needs at `scale`, deduplicated.
    ///
    /// This is the set an atlas must hold before the block can be drawn, computed
    /// in one pass so the packer can reserve space once instead of discovering
    /// misses mid-draw. `origin` is included because it changes the subpixel phase
    /// and therefore the images: a block at x = 10.0 and the same block at x = 10.5
    /// genuinely need different bitmaps.
    pub fn atlas_entries(
        &mut self,
        layout: &Layout,
        origin: (f32, f32),
        scale: f32,
    ) -> Vec<(GlyphKey, GlyphImage)> {
        let mut seen: HashMap<GlyphKey, ()> = HashMap::new();
        let mut entries = Vec::new();
        for glyph in layout.glyphs() {
            let key = glyph.physical(origin, scale).key;
            if seen.insert(key, ()).is_some() {
                continue;
            }
            if let Some(image) = self.rasterise(key) {
                entries.push((key, image));
            }
        }
        entries
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::{LayoutParams, tests::engine};
    use crate::span::StyledText;
    use std::collections::HashSet;

    fn laid_out(text: &str, font_size: f32) -> Layout {
        engine().layout(
            &StyledText::plain(text),
            &LayoutParams { font_size, ..LayoutParams::default() },
        )
    }

    #[test]
    fn a_rasterised_glyph_has_ink_and_a_sane_bitmap() {
        let layout = laid_out("H", 48.0);
        let physical = layout.lines[0].glyphs[0].physical((0.0, 0.0), 1.0);
        let image = engine().rasterise(physical.key).expect("H must rasterise");

        assert!(!image.is_blank(), "{image:?}");
        assert_eq!(image.content, GlyphContent::Coverage);
        assert_eq!(image.data.len(), image.row_bytes() * image.height as usize);
        // A cap-height glyph at 48px is roughly 34px tall; the bounds are loose
        // because the face is whatever the platform provides.
        assert!((10..80).contains(&image.height), "{image:?}");
        assert!(image.top > 0, "the bitmap must sit above the baseline: {image:?}");

        let ink: u32 = image.data.iter().map(|&v| v as u32).sum();
        assert!(ink > 0, "a rasterised H must contain coverage");
    }

    /// The offsets are what let the atlas store tight bitmaps, so they must not be
    /// silently baked into the pixels.
    #[test]
    fn the_bitmap_is_tight_and_placed_by_its_offsets() {
        let layout = laid_out("H", 64.0);
        let key = layout.lines[0].glyphs[0].physical((0.0, 0.0), 1.0).key;
        let image = engine().rasterise(key).unwrap();

        // Every edge row and column of a tight bitmap touches ink somewhere.
        let row_has_ink = |y: u32| (0..image.width).any(|x| image.coverage_at(x, y) != Some(0));
        let column_has_ink = |x: u32| (0..image.height).any(|y| image.coverage_at(x, y) != Some(0));
        assert!(row_has_ink(0) && row_has_ink(image.height - 1), "{image:?}");
        assert!(column_has_ink(0) && column_has_ink(image.width - 1), "{image:?}");
    }

    #[test]
    fn coverage_reads_are_bounds_checked() {
        let layout = laid_out("H", 32.0);
        let key = layout.lines[0].glyphs[0].physical((0.0, 0.0), 1.0).key;
        let image = engine().rasterise(key).unwrap();
        assert!(image.coverage_at(image.width, 0).is_none());
        assert!(image.coverage_at(0, image.height).is_none());
        assert!(image.coverage_at(0, 0).is_some());
    }

    /// A space has no ink. It must still rasterise, so the caller can distinguish
    /// it from a font that failed to load.
    #[test]
    fn a_blank_glyph_rasterises_to_an_empty_bitmap() {
        let layout = laid_out("a a", 32.0);
        let space = layout.lines[0].glyphs[1].physical((0.0, 0.0), 1.0).key;
        let image = engine().rasterise(space).expect("a space must still resolve");
        assert!(image.is_blank(), "{image:?}");
        assert!(image.data.is_empty());
    }

    /// The set of entries is exactly the set of distinct keys — nothing rasterised
    /// twice, nothing missing. A repeated letter usually still needs more than one
    /// slot, because its advance is fractional and each occurrence lands on a
    /// different subpixel phase; that is a genuine difference in the bitmap, not a
    /// dedup failure.
    #[test]
    fn atlas_entries_are_exactly_the_distinct_keys() {
        let layout = laid_out("aaaa", 32.0);
        let entries = engine().atlas_entries(&layout, (0.0, 0.0), 1.0);
        let distinct: HashSet<_> = layout.glyphs().map(|g| g.physical((0.0, 0.0), 1.0).key).collect();

        assert_eq!(layout.glyphs().count(), 4);
        assert_eq!(entries.len(), distinct.len());
        assert!(entries.len() <= 4, "at most one slot per subpixel phase: {}", entries.len());
        let returned: HashSet<_> = entries.iter().map(|(key, _)| *key).collect();
        assert_eq!(returned, distinct);
    }

    #[test]
    fn atlas_entries_cover_every_distinct_glyph() {
        let layout = laid_out("abc", 32.0);
        let entries = engine().atlas_entries(&layout, (0.0, 0.0), 1.0);
        assert_eq!(entries.len(), 3);
        let keys: Vec<_> = layout.glyphs().map(|g| g.physical((0.0, 0.0), 1.0).key).collect();
        for key in keys {
            assert!(entries.iter().any(|(k, _)| *k == key), "{key:?} missing from the atlas");
        }
    }

    /// The same text at twice the zoom is a different set of bitmaps — this is why
    /// a renderer must quantise scale rather than rebuilding the atlas per frame.
    #[test]
    fn scaling_produces_larger_distinct_bitmaps() {
        let layout = laid_out("H", 24.0);
        let mut engine = engine();
        let small = engine.rasterise(layout.lines[0].glyphs[0].physical((0.0, 0.0), 1.0).key).unwrap();
        let large = engine.rasterise(layout.lines[0].glyphs[0].physical((0.0, 0.0), 2.0).key).unwrap();
        assert!(large.width > small.width && large.height > small.height, "{small:?} {large:?}");
    }

    /// The bitmap caches have no eviction of their own, and their keys carry the
    /// device size — so every frame of a zoom gesture leaves a whole set behind that
    /// no later key can ever match. Sweeping a zoom without forgetting them grows the
    /// cache for the life of the process; this pins the release valve.
    #[test]
    fn forgetting_bitmaps_releases_every_scale_but_keeps_the_fonts() {
        let layout = laid_out("Hg", 24.0);
        let mut engine = engine();
        let faces = engine.face_count();

        // A slow zoom: every step is a distinct device size, and every one sticks.
        for step in 0..12 {
            let scale = 1.0 + step as f32 * 0.017;
            let _ = engine.atlas_entries(&layout, (0.0, 0.0), scale);
        }
        assert!(
            engine.glyph_bitmap_sizes().len() > 1,
            "a zoom sweep must actually mint distinct sizes, or this test proves nothing",
        );
        let swept = engine.glyph_bitmaps();
        assert!(swept > 2, "12 scales of 2 glyphs, not {swept}");

        engine.forget_glyph_bitmaps();
        assert_eq!(engine.glyph_bitmaps(), 0);
        assert_eq!(engine.face_count(), faces, "the fonts are not the thing being dropped");

        // And it still works afterwards: forgetting is a cache drop, not a teardown.
        let entries = engine.atlas_entries(&layout, (0.0, 0.0), 1.0);
        assert_eq!(entries.len(), 2, "H and g");
    }

    /// Every glyph in a real sticky must rasterise; a hole in the atlas renders as
    /// a missing character, which is the failure this guards.
    #[test]
    fn every_glyph_of_the_verified_sticky_rasterises() {
        let text = crate::from_miro_html("<p>fan</p><p><br /></p>");
        let mut engine = engine();
        let layout =
            engine.layout(&text, &LayoutParams { font_size: 36.0, ..LayoutParams::default() });
        let entries = engine.atlas_entries(&layout, (0.0, 0.0), 1.0);
        assert_eq!(entries.len(), 3, "f, a and n");
        assert!(entries.iter().all(|(_, image)| !image.is_blank()));
    }

    #[test]
    fn a_layout_with_no_glyphs_needs_no_atlas_entries() {
        let layout = laid_out("", 32.0);
        assert!(engine().atlas_entries(&layout, (0.0, 0.0), 1.0).is_empty());
    }
}
