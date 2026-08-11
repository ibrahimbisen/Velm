//! Measuring text, and remembering that you already did.
//!
//! # Why measurement is a trait
//!
//! Real measurement means shaping — `vellum-text`, and `cosmic-text` under it, with
//! a font database and a rasteriser behind that. This crate does not depend on any
//! of it. A table is a grid of rectangles; the only thing it needs from text is two
//! numbers, and taking a 40-crate dependency to get them would mean the table
//! model's tests could not run on a machine with no fonts, which is exactly the
//! machine CI runs on first.
//!
//! So [`Measure`] is the seam. `vellum-app` implements it over
//! `vellum_text::TextEngine`; [`MonospaceMeasure`] implements it over arithmetic,
//! and is what every test in this crate uses.
//!
//! The two must agree on one property, and only one: **an unbreakable word wider
//! than the line overflows rather than being split.** That is what
//! `cosmic_text::Wrap::Word` does, and layout leans on it — it is the difference
//! between a long URL sticking out of a narrow column, which is right, and a column
//! silently growing to 3000px, which is not.
//!
//! # The cache is where the performance claim lives
//!
//! [`MeasureCache`] holds two maps:
//!
//! | | keyed on | invalidated by |
//! |---|---|---|
//! | intrinsic widths | cell id + revision + style fingerprint | editing the cell, restyling it or its row |
//! | height at a width | the above **plus the width** | all of that, or that cell's column changing width |
//!
//! Intrinsic width is width-independent — see [`sizing`](crate::sizing) — so
//! resizing a column invalidates **no** intrinsic entry at all. Height entries are
//! keyed on width, so resizing column *j* invalidates exactly the cells in column
//! *j*. A resize therefore costs `O(rows)` fresh measurements on a table of any
//! width, never `O(rows × columns)`.
//!
//! [`MeasureStats`] exists so that claim is a test rather than a paragraph.
//!
//! The cache is keyed on cell **identity**, not position, so a cell that moved
//! because a row was inserted above it keeps its entries.
//!
//! One obligation on the caller: a cache belongs to one [`Measure`]. Swapping the
//! font stack underneath it — a new `TextEngine`, a font that finished loading —
//! means [`MeasureCache::clear`].

use std::collections::HashMap;

use crate::geometry::sanitise;
use crate::grid::{Cell, CellId};
use crate::span::StyledText;
use crate::style::TextStyle;

/// What a piece of text needs horizontally.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Intrinsic {
    /// The widest thing that cannot be broken — in practice the longest word. Text
    /// in a narrower box will stick out of it.
    pub min_content: f64,
    /// The width at which nothing wraps: the longest hard line.
    pub max_content: f64,
}

impl Intrinsic {
    pub fn new(min_content: f64, max_content: f64) -> Self {
        let max = sanitise(max_content);
        Self { min_content: sanitise(min_content).min(max), max_content: max }
    }
}

/// Text measurement, as this crate needs it.
///
/// `&mut self` because a real implementation owns a `FontSystem` and a shaping
/// cache, both of which mutate as they work.
pub trait Measure {
    /// The unwrapped and unbreakable widths of this text.
    fn intrinsic(&mut self, content: &StyledText, style: &TextStyle) -> Intrinsic;

    /// The height of this text laid out in exactly `width`, wrapping at word
    /// boundaries and letting an over-long word overflow rather than splitting it.
    ///
    /// Must return at least one line's height for empty text: an empty cell is still
    /// a cell, and a row of them still has a height.
    fn height(&mut self, content: &StyledText, style: &TextStyle, width: f64) -> f64;
}

/// A font-free stand-in for real shaping: every glyph is
/// [`advance_ratio`](Self::advance_ratio) times the font size wide.
///
/// This exists so the table model can be tested exhaustively without a font stack,
/// and so a headless tool — an export, a thumbnail, a test fixture — can lay a table
/// out at all. It is **not** an approximation of a proportional font and should
/// never reach the screen: use `vellum-text`.
///
/// Its wrapping matches `cosmic_text::Wrap::Word` on the one property layout depends
/// on, per the module docs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MonospaceMeasure {
    /// Advance width as a fraction of the font size. 0.6 is the usual ratio for a
    /// monospace face — SF Mono is 0.6 exactly.
    pub advance_ratio: f64,
}

impl Default for MonospaceMeasure {
    fn default() -> Self {
        Self { advance_ratio: 0.6 }
    }
}

impl MonospaceMeasure {
    pub fn new(advance_ratio: f64) -> Self {
        Self { advance_ratio: sanitise(advance_ratio) }
    }

    fn advance(&self, style: &TextStyle) -> f64 {
        let size = if style.font_size.is_finite() && style.font_size > 0.0 {
            style.font_size
        } else {
            crate::style::DEFAULT_FONT_SIZE
        };
        size * self.advance_ratio
    }

    /// Lines a paragraph occupies at `width`, wrapping greedily on whitespace.
    ///
    /// The first word on a line always goes on that line even when it does not fit,
    /// which is what makes an over-long word overflow instead of being split.
    fn line_count(paragraph: &str, advance: f64, width: f64) -> usize {
        if advance <= 0.0 {
            return 1;
        }
        let columns = (width / advance).floor() as isize;
        if columns < 1 {
            return paragraph.split_whitespace().count().max(1);
        }
        let columns = columns as usize;
        let mut lines = 1;
        let mut used = 0usize;
        for word in paragraph.split_whitespace() {
            let len = word.chars().count();
            if used == 0 {
                used = len;
            } else if used + 1 + len <= columns {
                used += 1 + len;
            } else {
                lines += 1;
                used = len;
            }
        }
        lines
    }
}

impl Measure for MonospaceMeasure {
    fn intrinsic(&mut self, content: &StyledText, style: &TextStyle) -> Intrinsic {
        let advance = self.advance(style);
        let plain = content.to_plain();
        let mut min_content: f64 = 0.0;
        let mut max_content: f64 = 0.0;
        for line in plain.split('\n') {
            max_content = max_content.max(line.chars().count() as f64 * advance);
            for word in line.split_whitespace() {
                min_content = min_content.max(word.chars().count() as f64 * advance);
            }
        }
        Intrinsic::new(min_content, max_content)
    }

    fn height(&mut self, content: &StyledText, style: &TextStyle, width: f64) -> f64 {
        let advance = self.advance(style);
        let plain = content.to_plain();
        let lines: usize =
            plain.split('\n').map(|p| Self::line_count(p, advance, width)).sum::<usize>().max(1);
        lines as f64 * style.line_advance()
    }
}

/// How much work the cache saved. Reported per instance and reset on demand, so a
/// test can assert what a specific edit cost.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MeasureStats {
    /// Intrinsic widths that reached the [`Measure`].
    pub intrinsic_measured: u64,
    /// Intrinsic widths served from the cache.
    pub intrinsic_cached: u64,
    /// Heights that reached the [`Measure`].
    pub height_measured: u64,
    /// Heights served from the cache.
    pub height_cached: u64,
}

impl MeasureStats {
    /// Everything that actually hit the text engine — the number a performance
    /// assertion cares about.
    pub fn total_measured(self) -> u64 {
        self.intrinsic_measured + self.height_measured
    }
}

/// Memoised text measurement. See the module docs for the keys and what invalidates
/// them.
#[derive(Debug, Clone, Default)]
pub struct MeasureCache {
    intrinsic: HashMap<(CellId, u64, u64), Intrinsic>,
    height: HashMap<(CellId, u64, u64, u64), f64>,
    stats: MeasureStats,
}

impl MeasureCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn intrinsic<M: Measure + ?Sized>(
        &mut self,
        measure: &mut M,
        cell: &Cell,
        style: &TextStyle,
    ) -> Intrinsic {
        let key = (cell.id(), cell.revision(), style.measurement_fingerprint());
        if let Some(hit) = self.intrinsic.get(&key) {
            self.stats.intrinsic_cached += 1;
            return *hit;
        }
        let value = measure.intrinsic(cell.content(), style);
        self.stats.intrinsic_measured += 1;
        self.intrinsic.insert(key, value);
        value
    }

    pub fn height<M: Measure + ?Sized>(
        &mut self,
        measure: &mut M,
        cell: &Cell,
        style: &TextStyle,
        width: f64,
    ) -> f64 {
        // Widths are produced by the same deterministic arithmetic every layout, so
        // an exact bit key hits whenever nothing moved. Quantising instead would
        // trade a guaranteed hit for a cache that occasionally answers about a
        // slightly different column.
        let width = sanitise(width);
        let key = (cell.id(), cell.revision(), style.measurement_fingerprint(), width.to_bits());
        if let Some(hit) = self.height.get(&key) {
            self.stats.height_cached += 1;
            return *hit;
        }
        let value = measure.height(cell.content(), style, width);
        self.stats.height_measured += 1;
        self.height.insert(key, value);
        value
    }

    pub fn stats(&self) -> MeasureStats {
        self.stats
    }

    pub fn reset_stats(&mut self) {
        self.stats = MeasureStats::default();
    }

    /// Number of memoised answers held. Entries for edited or deleted cells are
    /// never removed individually — see [`prune`](Self::prune).
    pub fn len(&self) -> usize {
        self.intrinsic.len() + self.height.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Drops everything. Required after the font stack changes underneath the cache,
    /// since the key cannot see that.
    pub fn clear(&mut self) {
        self.intrinsic.clear();
        self.height.clear();
    }

    /// Drops entries for cells not in `live`.
    ///
    /// Nothing is evicted on edit: a superseded revision simply stops being asked
    /// for. That is the right trade for a table, where the whole point is that a
    /// resize finds the previous answers still there — but it does mean a long
    /// editing session accumulates dead entries, and deleting a row leaks its cells'
    /// entries outright. The app calls this when it saves, or when the count gets
    /// silly. It is deliberately not automatic: an eviction that ran during layout
    /// would be doing work in the frame it exists to save.
    pub fn prune(&mut self, live: &[CellId]) {
        let live: std::collections::HashSet<CellId> = live.iter().copied().collect();
        self.intrinsic.retain(|(id, _, _), _| live.contains(id));
        self.height.retain(|(id, _, _, _), _| live.contains(id));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grid::{CellRef, Grid};

    fn styled(text: &str) -> StyledText {
        StyledText::plain(text)
    }

    /// One character is 13px × 0.6 = 7.8px; the ratio is the whole model.
    #[test]
    fn monospace_measures_the_longest_line_and_the_longest_word() {
        let mut m = MonospaceMeasure::default();
        let style = TextStyle::default();
        // The second line is 11 characters and unbreakable, so it sets both figures.
        let i = m.intrinsic(&styled("ab cd\nefghijklmno"), &style);
        assert!((i.max_content - 11.0 * 7.8).abs() < 1e-9);
        assert!((i.min_content - 11.0 * 7.8).abs() < 1e-9);

        let i = m.intrinsic(&styled("a bb ccc"), &style);
        assert!((i.max_content - 8.0 * 7.8).abs() < 1e-9);
        assert!((i.min_content - 3.0 * 7.8).abs() < 1e-9);
    }

    /// The property the real engine and this stand-in must agree on.
    #[test]
    fn an_unbreakable_word_overflows_rather_than_splitting() {
        let mut m = MonospaceMeasure::default();
        let style = TextStyle::default();
        let one_line = m.height(&styled("supercalifragilistic"), &style, 20.0);
        assert_eq!(one_line, style.line_advance(), "one word, one line, however narrow");
    }

    #[test]
    fn wrapping_is_greedy_on_whitespace() {
        let mut m = MonospaceMeasure::default();
        let style = TextStyle::default();
        // 7.8px per char, so 39px is five columns: "aa bb" fits, "cc" does not.
        let h = m.height(&styled("aa bb cc"), &style, 39.0);
        assert_eq!(h, 2.0 * style.line_advance());
    }

    #[test]
    fn empty_text_still_occupies_one_line() {
        let mut m = MonospaceMeasure::default();
        let style = TextStyle::default();
        assert_eq!(m.height(&StyledText::default(), &style, 100.0), style.line_advance());
        assert_eq!(m.intrinsic(&StyledText::default(), &style), Intrinsic::default());
    }

    #[test]
    fn hard_breaks_count_even_when_everything_fits() {
        let mut m = MonospaceMeasure::default();
        let style = TextStyle::default();
        assert_eq!(m.height(&styled("a\nb\nc"), &style, 1000.0), 3.0 * style.line_advance());
    }

    #[test]
    fn the_cache_answers_a_repeat_question_without_measuring() {
        let mut grid = Grid::new(1, 1);
        grid.cell_mut(CellRef::new(0, 0)).unwrap().set_content(styled("hello"));
        let cell = grid.cell(CellRef::new(0, 0)).unwrap();

        let mut m = MonospaceMeasure::default();
        let mut cache = MeasureCache::new();
        let style = TextStyle::default();

        let first = cache.intrinsic(&mut m, cell, &style);
        let second = cache.intrinsic(&mut m, cell, &style);
        assert_eq!(first, second);
        assert_eq!(cache.stats().intrinsic_measured, 1);
        assert_eq!(cache.stats().intrinsic_cached, 1);
    }

    #[test]
    fn editing_a_cell_invalidates_it_and_nothing_else() {
        let mut grid = Grid::new(1, 2);
        let mut m = MonospaceMeasure::default();
        let mut cache = MeasureCache::new();
        let style = TextStyle::default();
        for c in 0..2 {
            let cell = grid.cell(CellRef::new(0, c)).unwrap();
            cache.height(&mut m, cell, &style, 100.0);
        }
        cache.reset_stats();

        grid.cell_mut(CellRef::new(0, 0)).unwrap().set_content(styled("changed"));
        for c in 0..2 {
            let cell = grid.cell(CellRef::new(0, c)).unwrap();
            cache.height(&mut m, cell, &style, 100.0);
        }
        assert_eq!(cache.stats().height_measured, 1);
        assert_eq!(cache.stats().height_cached, 1);
    }

    /// Marking a row as a header bolds it without touching a single cell, so the
    /// style fingerprint is the only thing that can catch it.
    #[test]
    fn a_style_change_one_level_up_invalidates_the_cache() {
        let grid = Grid::new(1, 1);
        let cell = grid.cell(CellRef::new(0, 0)).unwrap();
        let mut m = MonospaceMeasure::default();
        let mut cache = MeasureCache::new();

        cache.intrinsic(&mut m, cell, &TextStyle::default());
        cache.intrinsic(&mut m, cell, &TextStyle { bold: true, ..TextStyle::default() });
        assert_eq!(cache.stats().intrinsic_measured, 2);
        assert_eq!(cache.stats().intrinsic_cached, 0);
    }

    #[test]
    fn pruning_drops_entries_for_cells_that_are_gone() {
        let grid = Grid::new(1, 2);
        let mut m = MonospaceMeasure::default();
        let mut cache = MeasureCache::new();
        let style = TextStyle::default();
        let kept = grid.cell(CellRef::new(0, 0)).unwrap().id();
        for c in 0..2 {
            cache.intrinsic(&mut m, grid.cell(CellRef::new(0, c)).unwrap(), &style);
        }
        assert_eq!(cache.len(), 2);
        cache.prune(&[kept]);
        assert_eq!(cache.len(), 1);
        cache.clear();
        assert!(cache.is_empty());
    }
}
