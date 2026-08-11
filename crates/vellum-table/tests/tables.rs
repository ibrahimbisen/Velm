//! The cases that are actually hard, exercised end to end through [`Table`] and
//! [`Table::layout`] rather than against one module at a time.
//!
//! Four of these are named in the crate's own brief because they are where a table
//! implementation normally breaks: deleting a row a merge starts on, auto-fitting a
//! word that cannot be broken, inserting a column into the middle of a merge, and a
//! table with no rows or no columns at all. The fifth —
//! `resizing_one_column_measures_only_that_column` — is the performance claim the
//! whole sizing design exists to make, asserted by counting how many times the text
//! engine is actually called.

use vellum_table::{
    BorderSide, CellRange, CellRef, Fit, HitTarget, Insets, MeasureCache, MergeContent,
    MonospaceMeasure, Orientation, Point, Sizing, StyledText, Table, TableLayout, VerticalAlign,
};

/// 13px text in a monospace stand-in whose advance is 0.6 of the size.
const CHAR: f64 = 13.0 * 0.6;
/// One line at Miro's default 1.36 line height.
const LINE: f64 = 13.0 * 1.36;
/// The default 8px padding, both sides.
const PAD: f64 = 16.0;

fn lay_out(table: &Table) -> TableLayout {
    table.layout(
        &mut MonospaceMeasure::default(),
        &mut MeasureCache::new(),
        Fit::Natural,
        Point::ORIGIN,
    )
}

fn lay_out_at(table: &Table, fit: Fit, origin: Point) -> TableLayout {
    table.layout(&mut MonospaceMeasure::default(), &mut MeasureCache::new(), fit, origin)
}

fn fill(table: &mut Table, prefix: &str) {
    for row in 0..table.row_count() {
        for col in 0..table.column_count() {
            if table.cell(CellRef::new(row, col)).is_some() {
                table.set_content(CellRef::new(row, col), format!("{prefix}{row}{col}")).unwrap();
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Merge then delete the row the merge starts on.
// ---------------------------------------------------------------------------

/// The orphan case. A merge anchored on the deleted row reaches past it, so naive
/// deletion leaves every position below pointing at a cell that no longer exists.
#[test]
fn deleting_the_row_a_merge_starts_on_keeps_the_merge_and_its_text() {
    let mut table = Table::new(4, 3);
    fill(&mut table, "");
    table.merge(CellRange::new(1, 0, 3, 2), MergeContent::KeepAnchor).unwrap();
    let id = table.cell(CellRef::new(1, 0)).unwrap().id();

    table.remove_row(1).unwrap();
    table.validate().unwrap();

    let survivor = table.cell(CellRef::new(1, 0)).expect("re-anchored one row down");
    assert_eq!(survivor.id(), id, "the same cell moved; it was not rebuilt");
    assert_eq!(survivor.span().rows, 2);
    assert_eq!(survivor.span().cols, 2);
    assert_eq!(survivor.content().to_plain(), "10");

    // And the layout agrees: 3 rows × 3 columns is 9 positions, 4 of them inside the
    // merge, so 6 cells.
    let layout = lay_out(&table);
    assert_eq!(layout.cells.len(), 6);
    let merged = layout.cell_at(CellRef::new(2, 1)).unwrap();
    assert_eq!(merged.anchor, CellRef::new(1, 0));
    assert_eq!(merged.rect.top(), layout.row_offsets[1]);
    assert_eq!(merged.rect.bottom(), layout.row_offsets[3]);
}

/// Deleting *every* row a merge touches must not leave anything behind either.
#[test]
fn deleting_a_merge_row_by_row_leaves_a_valid_table_at_each_step() {
    let mut table = Table::new(3, 3);
    table.merge(CellRange::new(0, 0, 3, 3), MergeContent::KeepAnchor).unwrap();
    assert_eq!(table.cell_count(), 1);

    for expected in [2, 1, 0] {
        table.remove_row(0).unwrap();
        table.validate().unwrap();
        assert_eq!(table.row_count(), expected);
    }
    assert_eq!(table.cell_count(), 0);
    assert_eq!(table.column_count(), 3, "the columns and their widths are still there");
}

#[test]
fn deleting_the_column_a_merge_starts_on_mirrors_the_row_case() {
    let mut table = Table::new(2, 4);
    fill(&mut table, "");
    table.merge(CellRange::new(0, 1, 2, 3), MergeContent::KeepAnchor).unwrap();
    table.remove_column(1).unwrap();
    table.validate().unwrap();

    let survivor = table.cell(CellRef::new(0, 1)).expect("re-anchored one column right");
    assert_eq!(survivor.span().cols, 2);
    assert_eq!(survivor.content().to_plain(), "01");
}

// ---------------------------------------------------------------------------
// Auto-fit with a single very long word.
// ---------------------------------------------------------------------------

/// A word cannot be broken, so auto-fit either widens the column to hold it or the
/// word sticks out. What must *not* happen is the row growing: wrapping a word that
/// has no break opportunity would be inventing one.
#[test]
fn auto_fit_widens_for_one_long_word_until_it_hits_the_cap() {
    let word: String = "K".repeat(100);
    let natural_width = 100.0 * CHAR + PAD;

    // Uncapped: the column simply becomes as wide as the word.
    let mut table = Table::new(1, 1);
    table.set_column_sizing(0, Sizing::Auto { min: 48.0, max: None }).unwrap();
    table.set_content(CellRef::new(0, 0), word.as_str()).unwrap();
    let layout = lay_out(&table);
    assert!((layout.column_widths[0] - natural_width).abs() < 1e-9);
    assert_eq!(layout.row_heights[0], LINE + PAD, "one line: nothing wrapped");
    assert!(!layout.cells[0].content_overflows);

    // Capped, which is the default for a column: the column stops at the cap and the
    // word is reported as overflowing rather than being broken or hidden.
    table.set_column_sizing(0, Sizing::Auto { min: 48.0, max: Some(480.0) }).unwrap();
    let layout = lay_out(&table);
    assert_eq!(layout.column_widths[0], 480.0);
    assert_eq!(layout.row_heights[0], LINE + PAD, "still one line: a word has no break");
    assert!(layout.cells[0].content_overflows, "the caller can offer to widen; we do not guess");
}

/// The contrast that makes the previous test mean something: text with break
/// opportunities wraps at the cap instead of overflowing, and the row grows.
#[test]
fn auto_fit_wraps_a_long_sentence_rather_than_overflowing() {
    let sentence = "aaaa ".repeat(40);
    let mut table = Table::new(1, 1);
    table.set_content(CellRef::new(0, 0), sentence.trim()).unwrap();
    let layout = lay_out(&table);

    assert_eq!(layout.column_widths[0], 480.0, "capped by the default auto column");
    assert!(!layout.cells[0].content_overflows);
    assert!(layout.row_heights[0] > 4.0 * LINE, "it wrapped onto many lines");
}

#[test]
fn an_empty_cell_still_occupies_a_row() {
    let table = Table::new(1, 1);
    let layout = lay_out(&table);
    assert_eq!(layout.row_heights[0], LINE + PAD);
    assert_eq!(layout.column_widths[0], 48.0, "the auto minimum");
}

// ---------------------------------------------------------------------------
// Insert a column inside an existing merge.
// ---------------------------------------------------------------------------

#[test]
fn inserting_a_column_inside_a_merge_widens_the_merged_cell() {
    let mut table = Table::new(2, 3);
    fill(&mut table, "");
    table.merge(CellRange::new(0, 0, 1, 3), MergeContent::KeepAnchor).unwrap();
    for i in 0..3 {
        table.set_column_sizing(i, Sizing::Fixed(100.0)).unwrap();
    }
    let before = lay_out(&table).cell_at(CellRef::new(0, 0)).unwrap().rect;
    assert_eq!(before.width(), 300.0);

    table.insert_column(1).unwrap();
    table.set_column_sizing(1, Sizing::Fixed(60.0)).unwrap();
    table.validate().unwrap();

    let layout = lay_out(&table);
    let merged = layout.cell_at(CellRef::new(0, 0)).unwrap();
    assert_eq!(merged.span.cols, 4);
    assert_eq!(merged.rect.width(), 360.0, "the merge grew by exactly the new column");
    assert_eq!(
        layout.cell_at(CellRef::new(1, 1)).unwrap().anchor,
        CellRef::new(1, 1),
        "below the merge the new column is an ordinary empty cell"
    );
    assert!(!layout.borders.iter().any(|b| b.orientation == Orientation::Vertical
        && b.boundary == 1
        && b.from.y < layout.row_offsets[1]));
}

/// Inserting at a merge's own left edge puts the column outside it, which is the
/// only reading of "insert a column here" that leaves the merge where the user can
/// see it.
#[test]
fn inserting_a_column_at_a_merge_boundary_leaves_it_alone() {
    let mut table = Table::new(1, 3);
    table.merge(CellRange::new(0, 1, 1, 2), MergeContent::KeepAnchor).unwrap();
    table.insert_column(1).unwrap();
    table.validate().unwrap();
    assert_eq!(table.cell(CellRef::new(0, 2)).unwrap().span().cols, 2);
    assert!(table.cell(CellRef::new(0, 1)).unwrap().span().is_single());
}

// ---------------------------------------------------------------------------
// Zero-row and zero-column tables.
// ---------------------------------------------------------------------------

#[test]
fn a_table_with_no_rows_has_no_area_but_keeps_its_columns() {
    let mut table = Table::new(0, 3);
    assert!(table.is_empty());
    table.set_column_sizing(1, Sizing::Fixed(120.0)).unwrap();
    table.validate().unwrap();

    let layout = lay_out(&table);
    assert_eq!(layout.size.height, 0.0);
    assert_eq!(layout.column_widths, vec![48.0, 120.0, 48.0], "widths resolve with no content");
    assert!(layout.cells.is_empty());
    assert!(layout.borders.is_empty(), "no rows, no edges to draw");
    assert_eq!(layout.hit_test(Point::new(50.0, 0.0), 4.0), HitTarget::Outside);

    // And the widths survive a row arriving.
    table.insert_row(0).unwrap();
    table.validate().unwrap();
    let layout = lay_out(&table);
    assert_eq!(layout.column_widths[1], 120.0);
    assert_eq!(layout.cells.len(), 3);
}

#[test]
fn a_table_with_no_columns_is_equally_harmless() {
    let mut table = Table::new(4, 0);
    assert!(table.is_empty());
    assert_eq!(table.cell_count(), 0);
    table.validate().unwrap();

    let layout = lay_out(&table);
    assert_eq!(layout.size.width, 0.0);
    assert_eq!(layout.row_heights.len(), 4);
    assert!(layout.borders.is_empty());
    assert_eq!(layout.cell_at(CellRef::new(0, 0)), None);

    table.insert_column(0).unwrap();
    table.validate().unwrap();
    assert_eq!(lay_out(&table).cells.len(), 4);
}

#[test]
fn a_table_with_nothing_at_all_lays_out_to_nothing() {
    let table = Table::new(0, 0);
    let layout = lay_out_at(&table, Fit::Width(500.0), Point::new(10.0, 20.0));
    assert_eq!(layout.size.width, 0.0, "there is no flexible track to reach 500 with");
    assert_eq!(layout.origin, Point::new(10.0, 20.0));
    assert!(layout.cells.is_empty());
    assert_eq!(layout.hit_test(Point::new(10.0, 20.0), 4.0), HitTarget::Outside);
}

// ---------------------------------------------------------------------------
// The performance claim.
// ---------------------------------------------------------------------------

/// `docs/01-architecture.md` §3: frame cost must scale with what changed, not with
/// what exists. For a table that means a column drag re-measures one column.
///
/// The numbers below are exact, not bounds, because a bound would pass if the cache
/// quietly stopped working in one direction.
#[test]
fn resizing_one_column_measures_only_that_column() {
    const ROWS: usize = 6;
    const COLUMNS: usize = 6;

    let mut table = Table::new(ROWS, COLUMNS);
    fill(&mut table, "cell ");
    let mut measure = MonospaceMeasure::default();
    let mut cache = MeasureCache::new();

    table.layout(&mut measure, &mut cache, Fit::Natural, Point::ORIGIN);
    let cold = cache.stats();
    assert_eq!(cold.intrinsic_measured, (ROWS * COLUMNS) as u64);
    assert_eq!(cold.height_measured, (ROWS * COLUMNS) as u64);

    // Re-laying out an unchanged table touches the text engine not at all.
    cache.reset_stats();
    table.layout(&mut measure, &mut cache, Fit::Natural, Point::ORIGIN);
    assert_eq!(cache.stats().total_measured(), 0);

    // One column resized: the six cells in it must rewrap, and nothing else may.
    cache.reset_stats();
    table.resize_column_to(2, 250.0).unwrap();
    table.layout(&mut measure, &mut cache, Fit::Natural, Point::ORIGIN);
    let stats = cache.stats();
    assert_eq!(stats.intrinsic_measured, 0, "intrinsic width does not depend on width");
    assert_eq!(stats.height_measured, ROWS as u64, "exactly the resized column");
    assert!(stats.height_cached >= ((ROWS * (COLUMNS - 1)) as u64));

    // Dragging it back gives back exactly the layout we started with — the sizing is
    // path-independent, which is only true because auto-fit ignores the available
    // width.
    let dragged_back = {
        let mut table = table.clone();
        table.set_column_sizing(2, Sizing::auto_column()).unwrap();
        table.layout(&mut measure, &mut cache, Fit::Natural, Point::ORIGIN)
    };
    let fresh = Table::new(ROWS, COLUMNS);
    let mut fresh = fresh;
    fill(&mut fresh, "cell ");
    assert_eq!(dragged_back.column_widths, lay_out(&fresh).column_widths);
}

/// Editing one cell must not re-measure the table either — only the cell, and only
/// the heights of the cells sharing a column width that moved as a result.
#[test]
fn editing_one_cell_measures_one_cell() {
    let mut table = Table::new(5, 5);
    fill(&mut table, "x");
    for i in 0..5 {
        table.set_column_sizing(i, Sizing::Fixed(100.0)).unwrap();
    }
    let mut measure = MonospaceMeasure::default();
    let mut cache = MeasureCache::new();
    table.layout(&mut measure, &mut cache, Fit::Natural, Point::ORIGIN);

    cache.reset_stats();
    table.set_content(CellRef::new(3, 3), "changed").unwrap();
    table.layout(&mut measure, &mut cache, Fit::Natural, Point::ORIGIN);
    assert_eq!(cache.stats().intrinsic_measured, 1);
    assert_eq!(cache.stats().height_measured, 1);
}

// ---------------------------------------------------------------------------
// Borders.
// ---------------------------------------------------------------------------

#[test]
fn a_plain_table_draws_each_grid_line_exactly_once() {
    let table = Table::new(3, 3);
    let layout = lay_out(&table);

    let vertical: Vec<_> =
        layout.borders.iter().filter(|b| b.orientation == Orientation::Vertical).collect();
    let horizontal: Vec<_> =
        layout.borders.iter().filter(|b| b.orientation == Orientation::Horizontal).collect();
    assert_eq!(vertical.len(), 4, "four column boundaries, each one coalesced run");
    assert_eq!(horizontal.len(), 4);
    assert!(vertical.iter().all(|b| (b.length() - layout.size.height).abs() < 1e-9));
    assert!(vertical.iter().all(|b| b.side == BorderSide::HAIRLINE));
}

#[test]
fn a_merged_block_has_no_lines_running_through_it() {
    let mut table = Table::new(3, 3);
    table.merge(CellRange::new(0, 0, 2, 2), MergeContent::KeepAnchor).unwrap();
    let layout = lay_out(&table);

    let interior: Vec<_> = layout
        .borders
        .iter()
        .filter(|b| b.orientation == Orientation::Vertical && b.boundary == 1)
        .collect();
    assert_eq!(interior.len(), 1, "boundary 1 survives only below the merge");
    assert_eq!(interior[0].from.y, layout.row_offsets[2]);
    assert_eq!(interior[0].to.y, layout.row_offsets[3]);
}

#[test]
fn a_thicker_border_wins_the_edge_it_shares_and_splits_the_run() {
    let mut table = Table::new(3, 2);
    let thick = BorderSide::new(3.0, None);
    table.cell_mut(CellRef::new(1, 0)).unwrap().style_mut().borders.right = Some(thick);
    let layout = lay_out(&table);

    let mut runs: Vec<_> = layout
        .borders
        .iter()
        .filter(|b| b.orientation == Orientation::Vertical && b.boundary == 1)
        .collect();
    runs.sort_by(|a, b| a.from.y.partial_cmp(&b.from.y).unwrap());
    assert_eq!(runs.len(), 3, "hairline, the thick middle, hairline");
    assert_eq!(runs[1].side, thick);
    assert_eq!(runs[1].from.y, layout.row_offsets[1]);
    assert_eq!(runs[1].to.y, layout.row_offsets[2]);
    assert_eq!(runs[0].side, BorderSide::HAIRLINE);
}

#[test]
fn turning_the_grid_lines_off_leaves_only_the_perimeter() {
    let mut table = Table::new(2, 2);
    table.style_mut().grid_border = None;
    let layout = lay_out(&table);
    assert_eq!(layout.borders.len(), 4, "one run per side of the table");
    assert!(layout.borders.iter().all(|b| b.boundary == 0 || b.boundary == 2));
}

// ---------------------------------------------------------------------------
// Rectangles, padding, alignment, fit.
// ---------------------------------------------------------------------------

#[test]
fn cells_tile_the_table_with_no_gaps_and_no_overlap() {
    let mut table = Table::new(3, 3);
    fill(&mut table, "");
    table.merge(CellRange::new(0, 0, 2, 2), MergeContent::KeepAnchor).unwrap();
    let layout = lay_out_at(&table, Fit::Natural, Point::new(1000.0, -40.0));

    let covered: f64 = layout.cells.iter().map(|c| c.rect.width() * c.rect.height()).sum();
    assert!((covered - layout.size.width * layout.size.height).abs() < 1e-6);
    assert_eq!(layout.cells[0].rect.origin, Point::new(1000.0, -40.0));
    assert_eq!(layout.bounds().right(), 1000.0 + layout.size.width);
}

#[test]
fn vertical_alignment_places_the_text_block_inside_the_cell() {
    let mut table = Table::new(1, 1);
    table.set_row_sizing(0, Sizing::Fixed(100.0)).unwrap();
    table.set_content(CellRef::new(0, 0), "x").unwrap();

    for (align, expected_top) in [
        (VerticalAlign::Top, 8.0),
        (VerticalAlign::Middle, 8.0 + (84.0 - LINE) / 2.0),
        (VerticalAlign::Bottom, 8.0 + (84.0 - LINE)),
    ] {
        table.style_mut().cell.vertical_align = align;
        let layout = lay_out(&table);
        let cell = &layout.cells[0];
        assert_eq!(cell.content_rect.top(), 8.0);
        assert_eq!(cell.content_rect.height(), 84.0, "100 less 8px of padding either side");
        assert!((cell.text_rect.top() - expected_top).abs() < 1e-9, "{align:?}");
        assert!((cell.text_rect.height() - LINE).abs() < 1e-9);
    }
}

#[test]
fn padding_comes_out_of_the_cell_not_off_its_neighbour() {
    let mut table = Table::new(1, 2);
    table.set_column_sizing(0, Sizing::Fixed(100.0)).unwrap();
    table.set_column_sizing(1, Sizing::Fixed(100.0)).unwrap();
    table.cell_mut(CellRef::new(0, 0)).unwrap().style_mut().padding = Some(Insets::uniform(20.0));
    let layout = lay_out(&table);

    assert_eq!(layout.cells[0].rect.right(), 100.0);
    assert_eq!(layout.cells[0].content_rect.right(), 80.0);
    assert_eq!(layout.cells[1].rect.left(), 100.0, "the neighbour is untouched");
    assert_eq!(layout.cells[1].content_rect.left(), 108.0);
}

#[test]
fn a_proportional_column_absorbs_the_width_the_others_do_not_want() {
    let mut table = Table::new(1, 3);
    table.set_column_sizing(0, Sizing::Fixed(100.0)).unwrap();
    table.set_column_sizing(1, Sizing::Fixed(100.0)).unwrap();
    table.set_column_sizing(2, Sizing::Proportional { weight: 1.0, min: 40.0 }).unwrap();

    let layout = lay_out_at(&table, Fit::Width(500.0), Point::ORIGIN);
    assert_eq!(layout.column_widths, vec![100.0, 100.0, 300.0]);
    assert_eq!(layout.size.width, 500.0);

    // Squeezed past what the fixed columns need, the proportional one stops at its
    // own floor and the table overflows rather than crushing a pinned column.
    let layout = lay_out_at(&table, Fit::Width(150.0), Point::ORIGIN);
    assert_eq!(layout.column_widths, vec![100.0, 100.0, 40.0]);
    assert_eq!(layout.size.width, 240.0, "wider than asked: compare the two to detect it");
}

#[test]
fn a_cell_spanning_columns_widens_the_auto_ones_it_crosses() {
    let mut table = Table::new(2, 3);
    table.merge(CellRange::new(0, 0, 1, 2), MergeContent::KeepAnchor).unwrap();
    table.set_content(CellRef::new(0, 0), "a".repeat(60).as_str()).unwrap();
    let layout = lay_out(&table);

    let merged = layout.cell_at(CellRef::new(0, 0)).unwrap();
    assert!(merged.rect.width() >= 60.0 * CHAR + PAD, "the span was given room");
    assert!(!merged.content_overflows);
    assert_eq!(layout.column_widths[2], 48.0, "the column it does not cross is untouched");
}

// ---------------------------------------------------------------------------
// Headers.
// ---------------------------------------------------------------------------

#[test]
fn a_header_row_is_bolder_and_wider_without_its_text_being_rewritten() {
    let mut table = Table::new(2, 1);
    table.set_content(CellRef::new(0, 0), "Torque").unwrap();
    let plain = lay_out(&table);
    assert!(!plain.cells[0].style.text.bold);

    table.set_header_rows(1);
    let with_header = lay_out(&table);
    assert!(with_header.cells[0].style.text.bold);
    assert_eq!(
        table.cell(CellRef::new(0, 0)).unwrap().content(),
        &StyledText::plain("Torque"),
        "the stored text is untouched, so clearing the header restores plain text"
    );

    // The spans handed to the text engine, on the other hand, are bold.
    let for_layout = table.cell(CellRef::new(0, 0)).unwrap().content().with_cell_defaults(
        with_header.cells[0].style.text.bold,
        false,
        None,
    );
    assert!(for_layout.spans().iter().all(|s| s.style.bold));
}

// ---------------------------------------------------------------------------
// Invariants under arbitrary editing.
// ---------------------------------------------------------------------------

/// The four grid invariants are only worth stating if they survive editing that
/// nobody wrote a case for. This applies a deterministic pseudo-random sequence of
/// every structural operation and validates after each one.
///
/// Deterministic on purpose: a failure here has to be reproducible from the seed
/// printed with it, not from a lucky rerun.
#[test]
fn every_structural_edit_in_any_order_leaves_a_valid_table() {
    for seed in 1..=32u64 {
        let mut rng = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15);
        let mut next = move || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng
        };
        let mut table = Table::new(4, 4);
        fill(&mut table, "");

        for step in 0..200 {
            let rows = table.row_count();
            let columns = table.column_count();
            let pick = |n: usize, r: u64| if n == 0 { 0 } else { (r % n as u64) as usize };
            let r = next();
            let _ = match r % 6 {
                0 if rows < 8 => table.insert_row(pick(rows + 1, next())),
                1 if columns < 8 => table.insert_column(pick(columns + 1, next())),
                2 if rows > 0 => table.remove_row(pick(rows, next())),
                3 if columns > 0 => table.remove_column(pick(columns, next())),
                4 if rows > 0 && columns > 0 => table
                    .merge(
                        CellRange::from_corners(
                            CellRef::new(pick(rows, next()), pick(columns, next())),
                            CellRef::new(pick(rows, next()), pick(columns, next())),
                        ),
                        MergeContent::Concatenate,
                    )
                    .map(|_| ()),
                _ if rows > 0 && columns > 0 => {
                    let at = CellRef::new(pick(rows, next()), pick(columns, next()));
                    match table.anchor_at(at) {
                        Some(anchor) => table.unmerge(anchor),
                        None => Ok(()),
                    }
                }
                _ => Ok(()),
            };
            table.validate().unwrap_or_else(|e| panic!("seed {seed}, step {step}: {e}"));

            // And it still lays out: every position resolves to exactly one cell.
            let layout = lay_out(&table);
            assert_eq!(layout.cells.len(), table.cell_count());
            for row in 0..table.row_count() {
                for col in 0..table.column_count() {
                    let at = CellRef::new(row, col);
                    let cell = layout.cell_at(at).expect("every position is covered");
                    assert_eq!(Some(cell.anchor), table.anchor_at(at));
                }
            }
        }
    }
}

/// Whatever the merges, hit-testing the centre of every position returns the cell
/// that owns it — the property the app's click handling depends on.
#[test]
fn hit_testing_agrees_with_the_grid_at_every_position() {
    let mut table = Table::new(4, 4);
    for i in 0..4 {
        table.set_column_sizing(i, Sizing::Fixed(80.0)).unwrap();
        table.set_row_sizing(i, Sizing::Fixed(40.0)).unwrap();
    }
    table.merge(CellRange::new(0, 0, 2, 2), MergeContent::KeepAnchor).unwrap();
    table.merge(CellRange::new(2, 2, 2, 2), MergeContent::KeepAnchor).unwrap();
    let layout = lay_out_at(&table, Fit::Natural, Point::new(-500.0, 250.0));

    for row in 0..4 {
        for col in 0..4 {
            let centre = Point::new(
                (layout.column_offsets[col] + layout.column_offsets[col + 1]) * 0.5,
                (layout.row_offsets[row] + layout.row_offsets[row + 1]) * 0.5,
            );
            assert_eq!(
                layout.hit_test(centre, 4.0),
                HitTarget::Cell { anchor: table.anchor_at(CellRef::new(row, col)).unwrap() },
                "at ({row}, {col})"
            );
        }
    }
}
