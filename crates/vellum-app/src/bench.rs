//! Synthetic boards for exercising the whole pipeline without a Miro paste.
//!
//! These are scaffolding, not a benchmark of the product. `docs/01-architecture.md`
//! §3 is explicit that the real gates are measured against the imported reference board,
//! because 100k untextured quads at 120fps is free on any modern GPU and proves
//! nothing about glyph-atlas pressure or texture residency.
//!
//! What changed when the app grew a document: this now builds a real
//! [`vellum_doc::Board`] of stickies rather than a bag of coloured rectangles. A
//! bench that skipped the document, the projection and the text engine would be
//! measuring a code path the product does not have.

use vellum_doc::{Board, ItemKind, NewItem, Placement, StyledText};

/// The extent of the reference Miro board, in world pixels, taken from its SVG
/// export. Synthetic boards are scaled to the same *density* so a 100k-item bench
/// lands on almost exactly these dimensions.
pub const REFERENCE_BOARD: (f64, f64) = (41_282.89, 17_515.36);

/// World pixels of board per item, derived from the reference board's area divided
/// by the 100k figure used for benchmarking. Holding this constant means a bench at
/// any `count` shows a comparable number of items on screen.
const AREA_PER_ITEM: f64 = (REFERENCE_BOARD.0 * REFERENCE_BOARD.1) / 100_000.0;

/// Miro's sticky palette, sRGB. Real colours rather than random noise so the bench
/// board reads as a board — and so a colour-management mistake is visible instead of
/// hiding in a wash of greys.
const PALETTE: [vellum_doc::Color; 6] = [
    vellum_doc::Color::rgb(0xFF, 0xF7, 0x9E), // the default yellow
    vellum_doc::Color::rgb(0xFF, 0x9E, 0x9E),
    vellum_doc::Color::rgb(0x9E, 0xDA, 0xFF),
    vellum_doc::Color::rgb(0xB1, 0xF0, 0xB1),
    vellum_doc::Color::rgb(0xDA, 0xBA, 0xFF),
    vellum_doc::Color::rgb(0xFF, 0xD3, 0x9E),
];

/// Words a bench sticky is labelled from. Short and real, so auto-fit has something
/// plausible to solve and the glyph atlas holds a realistic working set rather than
/// one repeated string.
const WORDS: [&str; 12] = [
    "coolant", "intake", "loom", "sensor", "harness", "throttle", "pump", "manifold", "relay",
    "filter", "bracket", "clamp",
];

/// Builds a board of `count` randomly placed stickies.
///
/// Deterministic for a given `seed`: two runs of `--bench 100000` must be comparable,
/// which they are not if the board changes between them.
pub fn scattered_board(count: usize, seed: u64) -> vellum_doc::Result<Board> {
    let aspect = REFERENCE_BOARD.0 / REFERENCE_BOARD.1;
    let area = (count.max(1) as f64) * AREA_PER_ITEM;
    let height = (area / aspect).sqrt();
    let width = height * aspect;

    let mut board = Board::new();
    board.set_title("Bench board")?;
    // One undo group: a bench board is a single act, and a hundred thousand undo
    // entries would dwarf the document they describe.
    board.begin_undo_group()?;
    let mut rng = SplitMix64::new(seed);
    for index in 0..count {
        let w = rng.range(80.0, 300.0);
        let h = rng.range(60.0, 220.0);
        let x = rng.range(-width / 2.0, width / 2.0);
        let y = rng.range(-height / 2.0, height / 2.0);
        let label = WORDS[index % WORDS.len()];
        board.add(NewItem::new(
            ItemKind::Sticky {
                text: StyledText::plain(label),
                background: Some(PALETTE[index % PALETTE.len()]),
            },
            Placement::new(x, y, w, h),
        ))?;
    }
    board.end_undo_group();
    Ok(board)
}

/// SplitMix64. Ten lines, no dependency, and identical output on every platform —
/// which is what makes a bench reproducible across the macOS and Windows CI runs.
struct SplitMix64(u64);

impl SplitMix64 {
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `[low, high)`.
    fn range(&mut self, low: f64, high: f64) -> f64 {
        let unit = (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64;
        low + unit * (high - low)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::Projection;

    #[test]
    fn a_bench_board_is_a_real_document() {
        let board = scattered_board(500, 1).unwrap();
        assert_eq!(board.item_count(), 500);
        assert_eq!(board.title(), "Bench board");

        let mut projection = Projection::new();
        projection.rebuild(&board).unwrap();
        assert_eq!(projection.len(), 500);
    }

    /// The density is what makes a bench at any count comparable: 100k items has to
    /// land on the reference board's own extent.
    #[test]
    fn a_hundred_thousand_items_lands_on_the_reference_board_extent() {
        let board = scattered_board(20_000, 1).unwrap();
        let mut projection = Projection::new();
        projection.rebuild(&board).unwrap();
        let content = projection.content_bounds().unwrap();

        let expected_area = 20_000.0 * AREA_PER_ITEM;
        let ratio = (content.width() * content.height()) / expected_area;
        assert!((0.8..1.4).contains(&ratio), "board area was {ratio}x the target");
    }

    #[test]
    fn the_same_seed_produces_the_same_board() {
        let placements = |seed| {
            let board = scattered_board(200, seed).unwrap();
            board
                .items()
                .unwrap()
                .into_iter()
                .map(|item| item.placement.x)
                .collect::<Vec<_>>()
        };
        assert_eq!(placements(42), placements(42), "seed 42 was not reproducible");
        assert_ne!(placements(42), placements(43), "different seeds agreed");
    }

    #[test]
    fn an_empty_bench_is_not_a_division_by_zero() {
        let board = scattered_board(0, 1).unwrap();
        assert!(board.is_empty());
        let mut projection = Projection::new();
        projection.rebuild(&board).unwrap();
        assert!(projection.content_bounds().is_none());
    }

    /// Every sticky carries a colour and a label, so auto-fit and the glyph atlas
    /// are both exercised rather than measured on an empty board.
    #[test]
    fn bench_stickies_are_coloured_and_labelled() {
        let board = scattered_board(24, 3).unwrap();
        let mut colours = std::collections::HashSet::new();
        for item in board.items().unwrap() {
            let ItemKind::Sticky { text, background } = item.kind else {
                panic!("a bench board holds stickies");
            };
            assert!(!text.is_empty());
            colours.insert(background.expect("a bench sticky is coloured"));
        }
        assert_eq!(colours.len(), PALETTE.len());
    }
}
