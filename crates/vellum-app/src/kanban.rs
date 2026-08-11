//! Kanban boards on the board: the document token, card measurement, and the laid-out
//! columns.
//!
//! `vellum-flow` owns the container: columns, WIP limits, fractional card ranks so a
//! move is one write, `layout(area)` into absolute rectangles, `hit_test`, and
//! `drop_target` — which returns both where a dragged card would land *and* the
//! rectangle to draw as a preview, with a test asserting the two agree exactly. As with
//! the other three structured widgets, all of that has been complete and tested since
//! before the document had anywhere to put one.
//!
//! # The token
//!
//! An opaque string, as [`crate::shapes`], [`crate::table`], [`crate::chart`] and
//! [`crate::mindmap`] use, for the layering reason those record.
//!
//! Unlike a mind map, a kanban's [`CardId`](vellum_flow::CardId)s are meaningful outside
//! the token: `vellum-flow`'s own note on [`Card`](vellum_flow::kanban::Card) says the id
//! is what would map to a board item once a card's text is a document rich-text
//! container. That is not what this does — a card's label lives in the token like
//! everything else here — and the note is worth keeping in view, because it is the shape
//! the finer grain would take.
//!
//! # The measurement
//!
//! `vellum-flow` never measures: a card's height is whatever the caller wrote, or
//! `KanbanMetrics::card_height` until one does. [`measured`] shapes each label at the
//! column width the layout will actually use and writes the height back, which is what
//! makes a three-line card three lines tall instead of overflowing a 64px default.
//!
//! That makes the layout **two-pass**, and it has to be: a card's height depends on the
//! column width, and the column width depends on the item's box and the number of
//! columns — but not on any height. So widths are settled first, then heights, then the
//! final layout. See [`layout`].
//!
//! # Honest limits
//!
//! - **Cards can be dragged between columns; nothing else can be edited.** No column can
//!   be added, renamed, reordered or given a WIP limit, and no card can be created,
//!   retitled or deleted. Every one of those exists and is tested in `vellum-flow`.
//! - **The board does not scroll.** `vellum-flow` deliberately lays overflow out past
//!   the edge and reports `overflows()` rather than squeezing; nothing here scrolls, so
//!   an overflowing board draws past its own bounds. Resizing the item is the remedy.

use vellum_flow::{
    CardId, Kanban, KanbanDrop, KanbanLayout, Point as FlowPoint, Rect as FlowRect, Slot,
};
use vellum_text::{LayoutParams, StyledText, TextEngine};

/// The token stored in the document for a kanban board.
pub fn encode(board: &Kanban) -> String {
    serde_json::to_string(board).unwrap_or_else(|error| {
        log::warn!("a kanban would not encode ({error}); storing nothing");
        String::new()
    })
}

/// The board a token names, or the default one when it cannot be read.
pub fn decode(token: &str) -> Kanban {
    serde_json::from_str(token).unwrap_or_else(|error| {
        if !token.is_empty() {
            log::warn!("unreadable kanban ({error}); drawing the default one");
        }
        default_kanban()
    })
}

/// Card and column label sizes, in world px.
///
/// A card's label is the 13px body size of `docs/05-design-language.md` §5 and a column
/// header is the same size in the muted ink, because a column title is a label and not a
/// heading — the count beside it is what gives the row its weight.
pub const CARD_FONT_SIZE: f64 = 13.0;
pub const HEADER_FONT_SIZE: f64 = 13.0;
pub const TITLE_FONT_SIZE: f64 = 15.0;

/// Padding inside a card, around its label.
pub const CARD_PADDING: f64 = 10.0;

/// The item's box when the kanban tool places one.
///
/// Wide enough for the default's three columns to clear `min_column_width` (180) with
/// their gaps and padding, and tall enough for four cards without overflowing — both
/// checked by a test, because a placed board that immediately overflows its own bounds
/// is the one thing this widget must not do out of the box.
pub const DEFAULT_SIZE: (f64, f64) = (720.0, 420.0);

/// What the kanban tool places: three columns with a few cards, and a WIP limit.
///
/// Not an empty board. Three empty columns are indistinguishable from a widget that
/// failed to load, there is no way to add a card yet, and the WIP limit is there because
/// a kanban without one is a list of lists — it is the constraint that makes it a kanban,
/// and `vellum-flow` renders a breach differently, which is worth being able to see.
pub fn default_kanban() -> Kanban {
    let mut board = Kanban::new("Sprint");
    let todo = board.add_column("To do");
    let doing = board.add_column("Doing");
    let done = board.add_column("Done");
    // Two against a limit of two: full, but not breached. A default that shipped
    // already over its own limit would read as an error state.
    let _ = board.set_wip_limit(doing, Some(2));
    for label in ["Measure the atlas", "Cull by band", "Fold the branches"] {
        let _ = board.add_card(todo, label);
    }
    for label in ["Shape the labels", "Route the connectors"] {
        let _ = board.add_card(doing, label);
    }
    let _ = board.add_card(done, "Pin the breakdown");
    board
}

/// Every word on the board, in reading order, for search and export.
///
/// The title, then each column's header and its cards' labels. A kanban's text lives
/// inside an opaque JSON token rather than in a `vellum_doc::StyledText`, so
/// `ItemKind::text` answers `None` for one and a search for a card's word used to find
/// nothing. See [`crate::words`].
pub fn words(board: &Kanban) -> Vec<String> {
    let mut out = vec![board.title().to_owned()];
    for column in board.columns() {
        out.push(column.title().to_owned());
        out.extend(column.cards().iter().map(|card| card.label().to_owned()));
    }
    out.retain(|text| !text.trim().is_empty());
    out
}

/// How wide a column will be, for a board of this shape laid out in this box.
///
/// Duplicating `vellum-flow`'s arithmetic would be two sources of truth for the number
/// every card's height depends on, so the width is *read back* from a first layout pass
/// instead. Cheap: the pass it runs is pure arithmetic over the cards' current heights,
/// and its heights are the ones about to be replaced.
fn column_content_width(board: &Kanban, size: (f64, f64)) -> f64 {
    let probe = board.layout(FlowRect::new(0.0, 0.0, size.0, size.1));
    probe
        .columns
        .first()
        .map_or(size.0, |column| column.body.width())
        .max(1.0)
}

/// The board with every card's height measured from its shaped label.
///
/// Returns a copy: a measurement is a property of the font stack this process happens to
/// have, not something to write back to the document. Same rule as
/// [`crate::mindmap::measured`].
pub fn measured(board: &Kanban, engine: &mut TextEngine, size: (f64, f64)) -> Kanban {
    let width = column_content_width(board, size);
    let wrap = (width - CARD_PADDING * 2.0).max(1.0);
    let mut out = board.clone();
    let ids: Vec<CardId> =
        out.columns().iter().flat_map(vellum_flow::kanban::Column::card_ids).collect();
    for id in ids {
        let Some(card) = out.card(id) else { continue };
        #[expect(clippy::cast_possible_truncation, reason = "a column's width is screen-scale")]
        let params = LayoutParams {
            font_size: CARD_FONT_SIZE as f32,
            max_width: Some(wrap as f32),
            ..LayoutParams::default()
        };
        let extent = engine.measure(&StyledText::plain(card.label()), &params);
        // At least the metrics' own default, so a one-word card is not shorter than the
        // rest and a board of them does not look ragged.
        let height = (f64::from(extent.height) + CARD_PADDING * 2.0)
            .max(out.metrics().card_height);
        let _ = out.set_card_height(id, Some(height));
    }
    out
}

/// The board laid out to fill `size`, with real card heights.
///
/// Returns the measured board alongside its layout because the two must not be allowed
/// to disagree: `KanbanLayout` carries rectangles and ids, and every label the drawing
/// needs is on the board those rectangles were computed from.
pub fn layout(board: &Kanban, engine: &mut TextEngine, size: (f64, f64)) -> (Kanban, KanbanLayout) {
    let measured = measured(board, engine, size);
    let laid = measured.layout(FlowRect::new(0.0, 0.0, size.0, size.1));
    (measured, laid)
}

/// Where a card dropped at a point in **board-local** coordinates would land.
///
/// A thin pass-through, and here rather than at the call site so that the coordinate
/// convention is stated once: `vellum-flow` works in the item's own space, with the
/// origin at its top-left, which is the same convention the table's cells and the mind
/// map's nodes use.
pub fn drop_target(laid: &KanbanLayout, local: (f64, f64), dragged: CardId) -> Option<KanbanDrop> {
    laid.drop_target(FlowPoint::new(local.0, local.1), Some(dragged))
}

/// What is under a point in board-local coordinates.
pub fn hit_test(laid: &KanbanLayout, local: (f64, f64)) -> vellum_flow::KanbanTarget {
    laid.hit_test(FlowPoint::new(local.0, local.1))
}

/// A world point in the item's own space: origin at its top-left, rotation undone.
///
/// The **inverse** of the placement, which is what makes a card on a rotated board still
/// hit-testable: `vellum-flow` works in an axis-aligned box and knows nothing about the
/// item's angle, so the angle has to come off before the point reaches it. Without this a
/// rotated kanban's cards could only be grabbed where the unrotated box happened to
/// overlap them.
pub fn board_local(placement: &vellum_doc::Placement, world: (f64, f64)) -> (f64, f64) {
    let (width, height) = placement.scaled_size();
    let (dx, dy) = (world.0 - placement.x, world.1 - placement.y);
    let (sin, cos) = (-placement.rotation).to_radians().sin_cos();
    (dx * cos - dy * sin + width / 2.0, dx * sin + dy * cos + height / 2.0)
}

/// A board-local rectangle back in world space, as four corners.
///
/// Four corners rather than a rect because a rotated board's card is not axis-aligned,
/// and the drop preview has to sit on the card it is previewing. Returned in
/// `[top-left, top-right, bottom-right, bottom-left]` order, so consecutive pairs are
/// the edges.
pub fn board_world(
    placement: &vellum_doc::Placement,
    rect: FlowRect,
) -> [(f64, f64); 4] {
    let (width, height) = placement.scaled_size();
    let (sin, cos) = placement.rotation.to_radians().sin_cos();
    let to_world = |x: f64, y: f64| {
        let (lx, ly) = (x - width / 2.0, y - height / 2.0);
        (placement.x + lx * cos - ly * sin, placement.y + lx * sin + ly * cos)
    };
    [
        to_world(rect.left(), rect.top()),
        to_world(rect.right(), rect.top()),
        to_world(rect.right(), rect.bottom()),
        to_world(rect.left(), rect.bottom()),
    ]
}

/// Commits a previewed drop.
///
/// `Slot::Index` rather than a rank computed here: `vellum-flow`'s own tests assert that
/// the index a `drop_target` reports puts the card exactly where its `preview` rectangle
/// was drawn, and re-deriving the position would give up that guarantee.
pub fn commit_drop(board: &mut Kanban, card: CardId, drop: &KanbanDrop) -> bool {
    board.move_card(card, drop.column, Slot::Index(drop.index)).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine() -> TextEngine {
        TextEngine::new().expect("the test machine has fonts")
    }

    #[test]
    fn a_kanban_survives_the_trip_to_the_document_and_back() {
        let board = default_kanban();
        assert_eq!(decode(&encode(&board)), board);
    }

    #[test]
    fn an_unreadable_token_becomes_the_default_board_rather_than_failing() {
        assert_eq!(decode("{ not json"), default_kanban());
        assert_eq!(decode(""), default_kanban());
    }

    /// The placed board is a board, not three empty columns. Nothing can add a card
    /// yet, so what is placed has to already read as a kanban.
    #[test]
    fn the_default_board_has_columns_cards_and_a_limit() {
        let board = default_kanban();
        assert_eq!(board.columns().len(), 3);
        assert_eq!(board.card_count(), 6);
        let limits: Vec<_> = board.columns().iter().map(vellum_flow::kanban::Column::wip_limit).collect();
        assert_eq!(limits, [None, Some(2), None]);
        // Full but not breached: a default that shipped over its own limit would read
        // as an error state rather than as a working example.
        assert!(board.breaches().is_empty(), "the default board starts in breach");
    }

    /// A box narrow enough that one column is near `min_column_width`, so labels
    /// actually wrap. A wide box is the wrong fixture for a wrapping test: with a single
    /// column in `DEFAULT_SIZE` the column is ~680px and nothing wraps at all, which is
    /// how the first version of these two tests came to fail on correct code.
    const NARROW: (f64, f64) = (240.0, 600.0);

    /// Six lines at 13px in a ~200px column, comfortably past the 64px floor.
    const LONG_LABEL: &str = "A label long enough to wrap over several lines in a column \
                              this narrow, which is what a real card on a real board \
                              actually looks like once somebody has written in it";

    /// The reason [`measured`] exists: `vellum-flow`'s default is one height for every
    /// card, so a long label overflows a 64px box silently.
    #[test]
    fn a_long_label_makes_a_taller_card() {
        let mut engine = engine();
        let mut board = Kanban::new("t");
        let column = board.add_column("c");
        let short = board.add_card(column, "One").unwrap();
        let long = board.add_card(column, LONG_LABEL).unwrap();

        let sized = measured(&board, &mut engine, NARROW);
        let height = |id| sized.card(id).unwrap().height().unwrap();
        assert!(height(long) > height(short), "{} vs {}", height(long), height(short));
        assert!(
            (height(short) - board.metrics().card_height).abs() < 1e-9,
            "a one-line card should be the metrics' own height, not shorter",
        );
    }

    /// A placed board must not overflow its own box, in either axis. This is the whole
    /// point of `DEFAULT_SIZE` being a checked number rather than a guess:
    /// `vellum-flow` lays overflow out past the edge on purpose and nothing here
    /// scrolls, so an overflowing default would draw outside the item it belongs to.
    #[test]
    fn the_default_board_fits_the_box_it_is_placed_in() {
        let mut engine = engine();
        let (_, laid) = layout(&default_kanban(), &mut engine, DEFAULT_SIZE);
        assert!(!laid.overflows(), "content {:?} in {:?}", laid.content, laid.rect.width());
        assert_eq!(laid.columns.len(), 3);
        for column in &laid.columns {
            assert!(
                column.rect.width() >= default_kanban().metrics().min_column_width,
                "column squeezed to {}",
                column.rect.width(),
            );
        }
    }

    /// Card heights come from shaping, so they must be *used* by the layout rather than
    /// measured and discarded — the failure mode that would leave every card 64px.
    #[test]
    fn measured_heights_reach_the_laid_out_rectangles() {
        let mut engine = engine();
        let mut board = Kanban::new("t");
        let column = board.add_column("c");
        let long = board.add_card(column, LONG_LABEL).unwrap();

        let (sized, laid) = layout(&board, &mut engine, NARROW);
        let expected = sized.card(long).unwrap().height().unwrap();
        let drawn = laid.card(long).unwrap().rect.height();
        assert!((drawn - expected).abs() < 1e-9, "drew {drawn}, measured {expected}");
        assert!(drawn > board.metrics().card_height, "the measurement was discarded");
    }

    /// The guarantee `vellum-flow` gives and this module must not break: the index a
    /// drop reports puts the card exactly where the preview was drawn.
    #[test]
    fn a_previewed_drop_lands_where_it_was_drawn() {
        let mut engine = engine();
        let mut board = default_kanban();
        let first = board.columns()[0].card_ids()[0];
        let (measured_board, laid) = layout(&board, &mut engine, DEFAULT_SIZE);

        // Aim at the top of the third column.
        let third = &laid.columns[2];
        let at = (third.body.centre().x, third.body.top() + 1.0);
        let drop = drop_target(&laid, at, first).expect("inside the board");
        assert_eq!(drop.column, third.id);
        assert_eq!(drop.index, 0);

        assert!(commit_drop(&mut board, first, &drop));
        assert_eq!(board.locate(first), Some((third.id, 0)));

        // And the card really occupies the rectangle the preview promised. Re-laid from
        // the *measured* board, so the only thing that changed is the move.
        let mut moved = measured_board;
        assert!(commit_drop(&mut moved, first, &drop));
        let after = moved.layout(FlowRect::new(0.0, 0.0, DEFAULT_SIZE.0, DEFAULT_SIZE.1));
        let landed = after.card(first).unwrap().rect;
        assert!((landed.top() - drop.preview.top()).abs() < 1e-9, "{landed:?} {:?}", drop.preview);
        assert!((landed.left() - drop.preview.left()).abs() < 1e-9);
    }

    /// A point outside the board is not a drop. Without this a drag that left the item
    /// would land the card back in the nearest column rather than doing nothing.
    #[test]
    fn a_drop_outside_the_board_is_no_drop_at_all() {
        let mut engine = engine();
        let board = default_kanban();
        let card = board.columns()[0].card_ids()[0];
        let (_, laid) = layout(&board, &mut engine, DEFAULT_SIZE);
        assert!(drop_target(&laid, (-50.0, -50.0), card).is_none());
        assert!(drop_target(&laid, (DEFAULT_SIZE.0 + 10.0, 10.0), card).is_none());
    }

    /// The two coordinate conversions are inverses, at any angle. Getting this wrong on
    /// a rotated board means a card can only be grabbed where the unrotated box happens
    /// to overlap it, which looks like the drag working intermittently.
    #[test]
    fn board_local_and_board_world_are_inverses_under_rotation() {
        use vellum_doc::Placement;
        for rotation in [0.0, 37.0, 90.0, 180.0, -45.0] {
            let placement =
                Placement { rotation, ..Placement::new(300.0, -120.0, 720.0, 420.0) };
            // A rect one corner of the board-local space, so both offsets are non-zero.
            let rect = FlowRect::new(40.0, 60.0, 100.0, 50.0);
            let corners = board_world(&placement, rect);
            let back = board_local(&placement, corners[0]);
            assert!(
                (back.0 - rect.left()).abs() < 1e-9 && (back.1 - rect.top()).abs() < 1e-9,
                "at {rotation}° the round trip gave {back:?} for {rect:?}",
            );
            // And the rectangle keeps its size, since rotation is rigid.
            let width = (corners[1].0 - corners[0].0).hypot(corners[1].1 - corners[0].1);
            assert!((width - rect.width()).abs() < 1e-9, "width became {width} at {rotation}°");
        }
    }

    /// Hit-testing reaches a card, its column and the chrome — what a press has to tell
    /// apart before it can decide whether a drag moves a card or the whole item.
    #[test]
    fn hit_testing_tells_a_card_from_the_chrome() {
        use vellum_flow::KanbanTarget;
        let mut engine = engine();
        let board = default_kanban();
        let (_, laid) = layout(&board, &mut engine, DEFAULT_SIZE);
        let column = &laid.columns[0];
        let card = column.cards[0];

        let centre = card.rect.centre();
        assert_eq!(
            hit_test(&laid, (centre.x, centre.y)),
            KanbanTarget::Card { column: column.id, card: card.id },
        );
        let header = column.header.centre();
        assert_eq!(hit_test(&laid, (header.x, header.y)), KanbanTarget::ColumnHeader(column.id));
        let title = laid.title.centre();
        assert_eq!(hit_test(&laid, (title.x, title.y)), KanbanTarget::Title);
        assert_eq!(hit_test(&laid, (-1.0, -1.0)), KanbanTarget::Outside);
    }
}
