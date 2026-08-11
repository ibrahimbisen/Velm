//! Kanban: named columns holding ordered cards, with optional WIP limits.
//!
//! ```
//! use vellum_flow::{Kanban, Slot, WipStatus};
//!
//! let mut board = Kanban::new("Sprint 14");
//! let todo = board.add_column("To do");
//! let doing = board.add_column("Doing");
//! board.set_wip_limit(doing, Some(2)).unwrap();
//!
//! let spec = board.add_card(todo, "Write the spec").unwrap();
//! let sdf = board.add_card(todo, "SDF corners").unwrap();
//! let ime = board.add_card(todo, "IME placement").unwrap();
//!
//! // Move the middle card to the top of another column.
//! board.move_card(sdf, doing, Slot::Top).unwrap();
//! assert_eq!(board.column(todo).unwrap().card_ids(), [spec, ime]);
//!
//! board.add_card(doing, "Glyph atlas").unwrap();
//! board.add_card(doing, "Texture eviction").unwrap();
//! assert_eq!(board.column(doing).unwrap().wip().status(), WipStatus::Over);
//! ```
//!
//! # Why a move is one write
//!
//! A card's position is a [`ColumnId`] and a [`Rank`] — a fractional key, exactly as
//! `docs/01-architecture.md` §4 does for z-order. [`Kanban::move_card`] writes those
//! two fields on the card being moved **and nothing else**: no other card in either
//! column changes by a byte, so the operation is one CRDT edit rather than one per
//! card below the insertion point, and undo is symmetrical with it.
//!
//! That is not a micro-optimisation. It is the difference between dropping a card
//! into a 200-card column costing one operation and costing two hundred, in a
//! document that stores, undoes and merges every one of them.
//!
//! # Why the cards live inside their column
//!
//! Each [`Column`] owns its `Vec<Card>` in rank order, and looking a card up by id
//! scans the columns. There is deliberately no id → column index alongside it: a
//! second structure would be a second source of truth to keep in step, and the
//! operations that need the lookup are single user actions — a click, the end of a
//! drag — not per-frame work. Layout and hit-testing never look a card up at all;
//! they walk the columns in order, which is what they want anyway.
//!
//! A kanban holding hundreds of thousands of cards would want the index. Adding it
//! would not change this API.

mod layout;

pub use layout::{CardLayout, ColumnLayout, KanbanDrop, KanbanLayout, KanbanTarget};

use serde::{Deserialize, Serialize};

use crate::error::FlowError;
use crate::id::{CardId, ColumnId, IdSource};
use crate::metrics::KanbanMetrics;
use crate::rank::Rank;
use crate::slot::{Ranked, Slot, rank_at};

/// A board of columns. The container itself.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Kanban {
    title: String,
    columns: Vec<Column>,
    metrics: KanbanMetrics,
    column_ids: IdSource,
    card_ids: IdSource,
}

/// One column: a name, an optional WIP limit, and its cards in order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Column {
    id: ColumnId,
    title: String,
    wip_limit: Option<usize>,
    /// Always in [`Rank`] order. Every insertion computes a key between the
    /// neighbours it is landing between, so keeping this sorted costs a `Vec::insert`
    /// and never a sort.
    cards: Vec<Card>,
}

/// One card.
///
/// The label is carried here so that a container is self-contained and testable
/// without a document behind it. When this is wired into `vellum-doc` the [`CardId`]
/// is what maps to a board item, and the item owns the styled text; the label stays
/// as what the container knows for measurement and export.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Card {
    id: CardId,
    label: String,
    rank: Rank,
    height: Option<f64>,
}

impl Ranked for Card {
    fn rank(&self) -> &Rank {
        &self.rank
    }
}

/// What [`Kanban::move_card`] wrote — the one card, and its two changed fields.
///
/// Returned rather than discarded so the caller can hand exactly this to the
/// document layer, and so a test can assert that a move really is one write.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CardMove {
    pub card: CardId,
    pub from: ColumnId,
    pub to: ColumnId,
    pub rank: Rank,
}

/// How full a column is against its limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Wip {
    pub count: usize,
    pub limit: Option<usize>,
}

/// The four states a column can be in, as a thing to draw.
///
/// Separate from [`Wip`] because the count and the limit are what the column shows
/// in its header, while the status is what colours it — `xr-red` for [`Over`], and
/// nothing at all for [`Unlimited`], which is most columns on most boards.
///
/// [`Over`]: WipStatus::Over
/// [`Unlimited`]: WipStatus::Unlimited
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WipStatus {
    /// No limit set.
    Unlimited,
    Under,
    /// Exactly at the limit. Worth showing distinctly: it is the moment the column
    /// stops accepting work, and a team that can see it coming does not overrun it.
    Full,
    Over,
}

/// A column that is over its limit, as reported by [`Kanban::breaches`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WipBreach {
    pub column: ColumnId,
    pub count: usize,
    pub limit: usize,
    /// How many cards over. `count - limit`, so the message can say "2 over" without
    /// the caller redoing the subtraction.
    pub overflow: usize,
}

impl Wip {
    pub fn status(self) -> WipStatus {
        match self.limit {
            None => WipStatus::Unlimited,
            Some(limit) if self.count > limit => WipStatus::Over,
            Some(limit) if self.count == limit => WipStatus::Full,
            Some(_) => WipStatus::Under,
        }
    }

    /// How many cards past the limit, or zero.
    pub fn overflow(self) -> usize {
        self.limit.map_or(0, |limit| self.count.saturating_sub(limit))
    }

    pub fn is_over(self) -> bool {
        self.overflow() > 0
    }
}

impl Card {
    pub fn id(&self) -> CardId {
        self.id
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    /// Its fractional position in its column. Public because the document layer has
    /// to persist it; nothing outside this crate should need to compare two.
    pub fn rank(&self) -> &Rank {
        &self.rank
    }

    /// The measured height, or `None` to use
    /// [`KanbanMetrics::card_height`](crate::KanbanMetrics::card_height).
    ///
    /// Measuring is the caller's job: this crate has no text engine, and pulling
    /// `vellum-text` in so that a layout container could measure a string would make
    /// every test of this crate depend on font loading.
    pub fn height(&self) -> Option<f64> {
        self.height
    }
}

impl Column {
    pub fn id(&self) -> ColumnId {
        self.id
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    pub fn wip_limit(&self) -> Option<usize> {
        self.wip_limit
    }

    /// The cards, in order, top to bottom.
    pub fn cards(&self) -> &[Card] {
        &self.cards
    }

    pub fn card_ids(&self) -> Vec<CardId> {
        self.cards.iter().map(Card::id).collect()
    }

    pub fn wip(&self) -> Wip {
        Wip { count: self.cards.len(), limit: self.wip_limit }
    }

    pub fn is_empty(&self) -> bool {
        self.cards.is_empty()
    }
}

impl Kanban {
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            columns: Vec::new(),
            metrics: KanbanMetrics::default(),
            column_ids: IdSource::default(),
            card_ids: IdSource::default(),
        }
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    pub fn set_title(&mut self, title: impl Into<String>) {
        self.title = title.into();
    }

    pub fn metrics(&self) -> KanbanMetrics {
        self.metrics
    }

    pub fn set_metrics(&mut self, metrics: KanbanMetrics) {
        self.metrics = metrics;
    }

    /// The columns, left to right.
    pub fn columns(&self) -> &[Column] {
        &self.columns
    }

    pub fn column(&self, id: ColumnId) -> Option<&Column> {
        self.columns.iter().find(|column| column.id == id)
    }

    pub fn column_index(&self, id: ColumnId) -> Option<usize> {
        self.columns.iter().position(|column| column.id == id)
    }

    pub fn card_count(&self) -> usize {
        self.columns.iter().map(|column| column.cards.len()).sum()
    }

    // ----- columns --------------------------------------------------------

    /// Appends a column on the right.
    pub fn add_column(&mut self, title: impl Into<String>) -> ColumnId {
        let index = self.columns.len();
        self.insert_column(index, title)
    }

    /// Inserts a column, clamping `index` to the end.
    ///
    /// Columns are a plain `Vec` and not fractionally indexed, unlike cards. A board
    /// has a handful of columns and reordering them is a rare, deliberate act on a
    /// list the user is looking at in full — whereas cards are many, are dragged
    /// constantly, and are the thing a renumbering would actually cost something on.
    pub fn insert_column(&mut self, index: usize, title: impl Into<String>) -> ColumnId {
        let id = ColumnId::from_raw(self.column_ids.next());
        let index = index.min(self.columns.len());
        self.columns.insert(
            index,
            Column { id, title: title.into(), wip_limit: None, cards: Vec::new() },
        );
        id
    }

    /// Removes a column and hands back everything that was in it.
    ///
    /// The cards come back rather than being dropped, because the caller has to
    /// decide: undo needs them, and "delete a column with cards in it" is a prompt in
    /// the UI, not a silent deletion.
    pub fn remove_column(&mut self, id: ColumnId) -> Result<Column, FlowError> {
        let index = self.column_index(id).ok_or(FlowError::NoSuchColumn(id))?;
        Ok(self.columns.remove(index))
    }

    pub fn rename_column(&mut self, id: ColumnId, title: impl Into<String>) -> Result<(), FlowError> {
        self.column_mut(id)?.title = title.into();
        Ok(())
    }

    /// Sets or clears a column's WIP limit.
    ///
    /// A limit of zero is allowed and means "nothing belongs here" — a holding column
    /// that should always be empty is a real pattern, and refusing it would be this
    /// crate having an opinion about someone's process.
    pub fn set_wip_limit(&mut self, id: ColumnId, limit: Option<usize>) -> Result<(), FlowError> {
        self.column_mut(id)?.wip_limit = limit;
        Ok(())
    }

    /// Moves a column to a new position, clamped to the end. The other columns keep
    /// their relative order and none of their cards move.
    pub fn move_column(&mut self, id: ColumnId, to_index: usize) -> Result<(), FlowError> {
        let from = self.column_index(id).ok_or(FlowError::NoSuchColumn(id))?;
        let to = to_index.min(self.columns.len() - 1);
        if from == to {
            return Ok(());
        }
        let column = self.columns.remove(from);
        self.columns.insert(to, column);
        Ok(())
    }

    // ----- cards ----------------------------------------------------------

    /// Appends a card to the bottom of a column.
    pub fn add_card(&mut self, column: ColumnId, label: impl Into<String>) -> Result<CardId, FlowError> {
        self.insert_card(column, label, Slot::Bottom)
    }

    pub fn insert_card(
        &mut self,
        column: ColumnId,
        label: impl Into<String>,
        slot: Slot,
    ) -> Result<CardId, FlowError> {
        let id = CardId::from_raw(self.card_ids.next());
        let label = label.into();
        let target = self.column_mut(column)?;
        let index = slot.index_in(target.cards.len());
        let rank = rank_at(&target.cards, index);
        target.cards.insert(index, Card { id, label, rank, height: None });
        Ok(id)
    }

    /// Moves a card to `to`, landing at `slot`.
    ///
    /// This is the operation the module docs are about: it writes the moved card's
    /// column and rank, and touches nothing else in either column. Moving a card to
    /// where it already is succeeds and still reports the write, so a drag that ends
    /// where it began is not a special case for the caller.
    ///
    /// A move that puts a column over its WIP limit is **allowed**, and shows up in
    /// [`Kanban::breaches`]. See [`crate::error`] for why that is not an error.
    pub fn move_card(
        &mut self,
        card: CardId,
        to: ColumnId,
        slot: Slot,
    ) -> Result<CardMove, FlowError> {
        let (from_index, card_index) = self.locate_indices(card).ok_or(FlowError::NoSuchCard(card))?;
        let to_index = self.column_index(to).ok_or(FlowError::NoSuchColumn(to))?;
        let from = self.columns[from_index].id;

        // Lift first, so `slot` counts positions in the list the card is landing in
        // rather than the one it is leaving. For a move within one column that is
        // the entire difference between the two readings of an index.
        let mut moved = self.columns[from_index].cards.remove(card_index);
        let destination = &mut self.columns[to_index];
        let index = slot.index_in(destination.cards.len());
        moved.rank = rank_at(&destination.cards, index);
        let rank = moved.rank.clone();
        destination.cards.insert(index, moved);
        Ok(CardMove { card, from, to, rank })
    }

    /// Removes a card and returns it, so a delete can be undone by re-inserting the
    /// same label at the same rank.
    pub fn remove_card(&mut self, card: CardId) -> Result<Card, FlowError> {
        let (column, index) = self.locate_indices(card).ok_or(FlowError::NoSuchCard(card))?;
        Ok(self.columns[column].cards.remove(index))
    }

    pub fn card(&self, card: CardId) -> Option<&Card> {
        self.columns.iter().flat_map(|column| &column.cards).find(|c| c.id == card)
    }

    /// Which column a card is in, and where in it.
    pub fn locate(&self, card: CardId) -> Option<(ColumnId, usize)> {
        let (column, index) = self.locate_indices(card)?;
        Some((self.columns[column].id, index))
    }

    /// Records a measured height. `None` puts the card back on the default.
    pub fn set_card_height(&mut self, card: CardId, height: Option<f64>) -> Result<(), FlowError> {
        self.card_mut(card)?.height = height.map(|h| h.max(0.0));
        Ok(())
    }

    pub fn set_card_label(&mut self, card: CardId, label: impl Into<String>) -> Result<(), FlowError> {
        self.card_mut(card)?.label = label.into();
        Ok(())
    }

    // ----- WIP ------------------------------------------------------------

    /// Every column that is over its limit, left to right. Empty on a healthy board,
    /// which is the common case and costs one pass over the columns.
    pub fn breaches(&self) -> Vec<WipBreach> {
        self.columns
            .iter()
            .filter_map(|column| {
                let wip = column.wip();
                let limit = wip.limit?;
                let overflow = wip.overflow();
                (overflow > 0).then_some(WipBreach { column: column.id, count: wip.count, limit, overflow })
            })
            .collect()
    }

    // ----- internals ------------------------------------------------------

    fn column_mut(&mut self, id: ColumnId) -> Result<&mut Column, FlowError> {
        self.columns.iter_mut().find(|column| column.id == id).ok_or(FlowError::NoSuchColumn(id))
    }

    fn card_mut(&mut self, card: CardId) -> Result<&mut Card, FlowError> {
        let (column, index) = self.locate_indices(card).ok_or(FlowError::NoSuchCard(card))?;
        Ok(&mut self.columns[column].cards[index])
    }

    /// Column index and card index, the pair every mutation needs. Private because
    /// positional indices are not a handle anyone outside should hold — they are
    /// invalidated by the very next edit, which is what [`CardId`] exists to avoid.
    fn locate_indices(&self, card: CardId) -> Option<(usize, usize)> {
        self.columns.iter().enumerate().find_map(|(column, c)| {
            c.cards.iter().position(|existing| existing.id == card).map(|index| (column, index))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A board with three columns and cards named for their column, which is what
    /// makes the ordering assertions below readable.
    fn board() -> (Kanban, Vec<ColumnId>) {
        let mut board = Kanban::new("Sprint 14");
        let columns: Vec<ColumnId> =
            ["To do", "Doing", "Done"].into_iter().map(|t| board.add_column(t)).collect();
        for (index, &column) in columns.iter().enumerate() {
            for card in 0..3 {
                board.add_card(column, format!("c{index}-{card}")).unwrap();
            }
        }
        (board, columns)
    }

    fn labels(board: &Kanban, column: ColumnId) -> Vec<String> {
        board.column(column).unwrap().cards().iter().map(|c| c.label().to_owned()).collect()
    }

    /// The headline test. Moving a card must leave every other card exactly as it
    /// was — same order, and, more strongly, the same rank byte for byte, which is
    /// what "one write" means at the document layer.
    #[test]
    fn moving_a_card_between_columns_reindexes_nothing_else() {
        let (mut board, columns) = board();
        let moved = board.column(columns[0]).unwrap().cards()[1].id();

        let before: Vec<(CardId, Rank)> = board
            .columns()
            .iter()
            .flat_map(|c| c.cards())
            .filter(|c| c.id() != moved)
            .map(|c| (c.id(), c.rank().clone()))
            .collect();

        let write = board.move_card(moved, columns[2], Slot::Index(1)).unwrap();
        assert_eq!(write.from, columns[0]);
        assert_eq!(write.to, columns[2]);

        let after: Vec<(CardId, Rank)> = board
            .columns()
            .iter()
            .flat_map(|c| c.cards())
            .filter(|c| c.id() != moved)
            .map(|c| (c.id(), c.rank().clone()))
            .collect();
        assert_eq!(before, after, "no other card's rank or order may change");

        assert_eq!(labels(&board, columns[0]), ["c0-0", "c0-2"]);
        assert_eq!(labels(&board, columns[2]), ["c2-0", "c0-1", "c2-1", "c2-2"]);
        assert_eq!(board.locate(moved), Some((columns[2], 1)));
        assert_eq!(board.card_count(), 9, "a move is not a copy and not a delete");
    }

    /// Cards stay sorted by rank, in every column, after any sequence of edits. If
    /// this ever failed, the order on screen would depend on insertion history
    /// rather than on the keys.
    #[test]
    fn every_column_stays_in_rank_order() {
        let (mut board, columns) = board();
        let ids: Vec<CardId> =
            board.columns().iter().flat_map(|c| c.card_ids()).collect();
        for (n, card) in ids.iter().enumerate() {
            let target = columns[n % columns.len()];
            board.move_card(*card, target, Slot::Index(n % 4)).unwrap();
        }
        for column in board.columns() {
            assert!(
                column.cards().windows(2).all(|w| w[0].rank() < w[1].rank()),
                "{} is out of rank order",
                column.title()
            );
        }
    }

    /// Within one column, an index counts the list with the card already lifted out.
    #[test]
    fn a_move_within_a_column_reads_its_index_after_the_card_is_lifted() {
        let (mut board, columns) = board();
        let first = board.column(columns[0]).unwrap().cards()[0].id();
        board.move_card(first, columns[0], Slot::Index(1)).unwrap();
        assert_eq!(labels(&board, columns[0]), ["c0-1", "c0-0", "c0-2"]);

        // And dragging it back to the top is the inverse.
        board.move_card(first, columns[0], Slot::Top).unwrap();
        assert_eq!(labels(&board, columns[0]), ["c0-0", "c0-1", "c0-2"]);
    }

    #[test]
    fn a_move_to_where_the_card_already_is_is_not_a_special_case() {
        let (mut board, columns) = board();
        let card = board.column(columns[1]).unwrap().cards()[1].id();
        board.move_card(card, columns[1], Slot::Index(1)).unwrap();
        assert_eq!(labels(&board, columns[1]), ["c1-0", "c1-1", "c1-2"]);
    }

    #[test]
    fn slots_place_at_the_top_bottom_and_between() {
        let mut board = Kanban::new("t");
        let column = board.add_column("only");
        board.insert_card(column, "b", Slot::Bottom).unwrap();
        board.insert_card(column, "a", Slot::Top).unwrap();
        board.insert_card(column, "ab", Slot::Index(1)).unwrap();
        board.insert_card(column, "z", Slot::Index(99)).unwrap();
        assert_eq!(labels(&board, column), ["a", "ab", "b", "z"]);
    }

    #[test]
    fn removing_a_column_hands_back_its_cards_rather_than_dropping_them() {
        let (mut board, columns) = board();
        let removed = board.remove_column(columns[1]).unwrap();
        assert_eq!(removed.cards().len(), 3);
        assert_eq!(removed.title(), "Doing");
        assert_eq!(board.columns().len(), 2);
        assert_eq!(board.card_count(), 6);
        // Its cards are gone from the board, and their ids now match nothing.
        let orphan = removed.cards()[0].id();
        assert_eq!(board.card(orphan), None);
        assert_eq!(board.move_card(orphan, columns[0], Slot::Top), Err(FlowError::NoSuchCard(orphan)));
    }

    #[test]
    fn columns_reorder_without_touching_their_cards() {
        let (mut board, columns) = board();
        board.move_column(columns[2], 0).unwrap();
        assert_eq!(
            board.columns().iter().map(Column::title).collect::<Vec<_>>(),
            ["Done", "To do", "Doing"]
        );
        assert_eq!(labels(&board, columns[2]), ["c2-0", "c2-1", "c2-2"]);
        // Past the end clamps to last.
        board.move_column(columns[2], 99).unwrap();
        assert_eq!(board.column_index(columns[2]), Some(2));
    }

    #[test]
    fn renaming_and_limits_apply_to_the_named_column_only() {
        let (mut board, columns) = board();
        board.rename_column(columns[1], "In progress").unwrap();
        board.set_wip_limit(columns[1], Some(2)).unwrap();
        assert_eq!(board.column(columns[1]).unwrap().title(), "In progress");
        assert_eq!(board.column(columns[0]).unwrap().wip_limit(), None);
    }

    #[test]
    fn a_wip_limit_reports_rather_than_refuses() {
        let (mut board, columns) = board();
        board.set_wip_limit(columns[1], Some(3)).unwrap();
        assert_eq!(board.column(columns[1]).unwrap().wip().status(), WipStatus::Full);
        assert!(board.breaches().is_empty(), "at the limit is not over it");

        let card = board.column(columns[0]).unwrap().cards()[0].id();
        board.move_card(card, columns[1], Slot::Bottom).expect("the move is allowed");

        let breaches = board.breaches();
        assert_eq!(
            breaches,
            [WipBreach { column: columns[1], count: 4, limit: 3, overflow: 1 }]
        );
        assert_eq!(board.column(columns[1]).unwrap().wip().status(), WipStatus::Over);
        assert!(board.column(columns[1]).unwrap().wip().is_over());
    }

    #[test]
    fn an_unlimited_column_never_breaches_and_a_zero_limit_always_does() {
        let (mut board, columns) = board();
        assert_eq!(board.column(columns[0]).unwrap().wip().status(), WipStatus::Unlimited);
        assert_eq!(board.column(columns[0]).unwrap().wip().overflow(), 0);
        board.set_wip_limit(columns[0], Some(0)).unwrap();
        assert_eq!(board.column(columns[0]).unwrap().wip().overflow(), 3);
    }

    /// A refused edit must leave the board byte-identical, not merely return an
    /// error — half-applying a move is how a card gets lost.
    #[test]
    fn a_refused_edit_changes_nothing() {
        let (mut board, columns) = board();
        let snapshot = serde_json::to_string(&board).unwrap();
        let stale = ColumnId::from_raw(9999);
        let ghost = CardId::from_raw(9999);
        let card = board.column(columns[0]).unwrap().cards()[0].id();

        assert_eq!(board.move_card(card, stale, Slot::Top), Err(FlowError::NoSuchColumn(stale)));
        assert_eq!(board.move_card(ghost, columns[0], Slot::Top), Err(FlowError::NoSuchCard(ghost)));
        assert_eq!(board.remove_column(stale).unwrap_err(), FlowError::NoSuchColumn(stale));
        assert_eq!(board.remove_card(ghost).unwrap_err(), FlowError::NoSuchCard(ghost));
        assert_eq!(board.rename_column(stale, "x").unwrap_err(), FlowError::NoSuchColumn(stale));
        assert_eq!(board.set_card_height(ghost, Some(1.0)).unwrap_err(), FlowError::NoSuchCard(ghost));

        assert_eq!(serde_json::to_string(&board).unwrap(), snapshot);
    }

    #[test]
    fn a_board_survives_a_save_and_load_unchanged() {
        let (mut board, columns) = board();
        board.set_wip_limit(columns[1], Some(2)).unwrap();
        let card = board.column(columns[0]).unwrap().cards()[2].id();
        board.move_card(card, columns[1], Slot::Index(1)).unwrap();
        board.set_card_height(card, Some(96.0)).unwrap();

        let json = serde_json::to_string(&board).unwrap();
        let loaded: Kanban = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded, board);
        assert_eq!(serde_json::to_string(&loaded).unwrap(), json);
    }

    /// Ids are not reused, so a card added after a deletion cannot collide with the
    /// handle a stale drag is still holding.
    #[test]
    fn ids_are_not_reused_after_a_deletion() {
        let (mut board, columns) = board();
        let card = board.column(columns[0]).unwrap().cards()[0].id();
        board.remove_card(card).unwrap();
        let replacement = board.add_card(columns[0], "new").unwrap();
        assert_ne!(replacement, card);
        assert_eq!(board.card(card), None);
    }
}
