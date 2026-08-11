//! Turning a kanban into rectangles, and turning a point back into a card.
//!
//! Layout is a pure function of the board, the metrics and the area — no cache, no
//! dirty flags. It is a walk over the columns and their cards with two running sums,
//! which is cheaper than deciding whether a cached result is still valid.
//!
//! # Overflow is reported, not resolved
//!
//! Columns never shrink below
//! [`min_column_width`](crate::KanbanMetrics::min_column_width), and cards never
//! shrink at all. When the board needs more room than the area gives it,
//! [`KanbanLayout::content`] exceeds the area and the extra columns and cards are
//! laid out past its edge. Scrolling and clipping belong to whatever owns the
//! viewport; a layout that quietly compressed twelve columns into 600px would be
//! producing a board nobody can read, and hiding the fact.

use crate::geometry::{Point, Rect, Size};
use crate::id::{CardId, ColumnId};
use crate::kanban::{Card, Kanban, Wip};
use crate::metrics::KanbanMetrics;

/// Absolute rectangles for a whole board.
#[derive(Debug, Clone, PartialEq)]
pub struct KanbanLayout {
    /// The area the board was laid out in.
    pub rect: Rect,
    /// The container's title strip.
    pub title: Rect,
    /// Inside the padding, where the columns live.
    pub body: Rect,
    pub columns: Vec<ColumnLayout>,
    /// What the board needs. Larger than `rect` on either axis when it overflows.
    pub content: Size,
    metrics: KanbanMetrics,
}

/// One column's chrome and children.
#[derive(Debug, Clone, PartialEq)]
pub struct ColumnLayout {
    pub id: ColumnId,
    /// The whole column, header included.
    pub rect: Rect,
    /// Title, count and WIP badge.
    pub header: Rect,
    /// Where the cards go, inside the column's padding.
    pub body: Rect,
    pub cards: Vec<CardLayout>,
    /// Carried so a renderer can colour the header from the layout alone, without
    /// going back to the model for a number the layout already knows.
    pub wip: Wip,
    /// The height the cards need. Greater than `body.height()` when the column
    /// overflows.
    pub content_height: f64,
}

/// One card's rectangle.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CardLayout {
    pub id: CardId,
    pub rect: Rect,
}

/// What is under a point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KanbanTarget {
    /// Not on the board at all.
    Outside,
    /// The container's title strip.
    Title,
    /// Inside the board but not on a column — the padding, or a gap between two.
    Board,
    ColumnHeader(ColumnId),
    /// A column, but not one of its cards. Where a click starts a marquee and a drop
    /// lands at the end.
    ColumnBody(ColumnId),
    Card { column: ColumnId, card: CardId },
}

/// Where a dragged card would land, and the gap it would land in.
///
/// The index is a [`Slot::Index`](crate::Slot::Index): pass it straight to
/// [`Kanban::move_card`] and the card ends up exactly where `preview` was drawn.
/// That is asserted, not assumed — see the tests.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct KanbanDrop {
    pub column: ColumnId,
    pub index: usize,
    /// The slot that would open, in absolute coordinates: draw a placeholder or an
    /// insertion line here.
    pub preview: Rect,
}

impl Kanban {
    /// Absolute rectangles for the container chrome and every child, in `area`.
    pub fn layout(&self, area: Rect) -> KanbanLayout {
        let m = self.metrics();
        let (title, below) = area.split_top(m.title_height);
        let body = below.inset_by(m.padding);

        let count = self.columns().len();
        let column_width = if count == 0 {
            0.0
        } else {
            let gaps = m.column_gap * (count - 1) as f64;
            ((body.width() - gaps) / count as f64).max(m.min_column_width)
        };

        let mut columns = Vec::with_capacity(count);
        let mut tallest = 0.0f64;
        for (index, column) in self.columns().iter().enumerate() {
            let left = body.left() + index as f64 * (column_width + m.column_gap);
            let rect = Rect::new(left, body.top(), column_width, body.height());
            let (header, under) = rect.split_top(m.column_header_height);
            let column_body = under.inset_by(m.column_padding);

            let mut cards = Vec::with_capacity(column.cards().len());
            let mut y = column_body.top();
            for card in column.cards() {
                let height = card_height(card, &m);
                cards.push(CardLayout {
                    id: card.id(),
                    rect: Rect::new(column_body.left(), y, column_body.width(), height),
                });
                y += height + m.card_gap;
            }

            let stack = stack_height(column.cards(), &m);
            let content_height = m.column_header_height + 2.0 * m.column_padding + stack;
            tallest = tallest.max(content_height);
            columns.push(ColumnLayout {
                id: column.id(),
                rect,
                header,
                body: column_body,
                cards,
                wip: column.wip(),
                content_height,
            });
        }

        let content_width = if count == 0 {
            2.0 * m.padding
        } else {
            2.0 * m.padding + count as f64 * column_width + (count - 1) as f64 * m.column_gap
        };
        let content = Size::new(content_width, m.title_height + 2.0 * m.padding + tallest);

        KanbanLayout { rect: area, title, body, columns, content, metrics: m }
    }
}

/// A card's laid-out height: what the caller measured, or the default.
fn card_height(card: &Card, metrics: &KanbanMetrics) -> f64 {
    card.height().filter(|h| *h > 0.0).unwrap_or(metrics.card_height)
}

/// The height a run of cards occupies, gaps included.
fn stack_height(cards: &[Card], metrics: &KanbanMetrics) -> f64 {
    let heights: f64 = cards.iter().map(|card| card_height(card, metrics)).sum();
    let gaps = metrics.card_gap * cards.len().saturating_sub(1) as f64;
    heights + gaps
}

impl KanbanLayout {
    pub fn column(&self, id: ColumnId) -> Option<&ColumnLayout> {
        self.columns.iter().find(|column| column.id == id)
    }

    pub fn card(&self, id: CardId) -> Option<&CardLayout> {
        self.columns.iter().flat_map(|column| &column.cards).find(|card| card.id == id)
    }

    /// True when the board needs more room than it was given, on either axis.
    pub fn overflows(&self) -> bool {
        self.content.width > self.rect.width() || self.content.height > self.rect.height()
    }

    /// What is under `point`.
    ///
    /// Only the area the board was laid out in is tested: a point past its edge is
    /// [`Outside`](KanbanTarget::Outside) even if an overflowing column drew a card
    /// there. Whether that overflow is scrolled, clipped or given a bigger area is
    /// the caller's decision, and this function cannot answer differently depending
    /// on which it chose.
    ///
    /// Within the area, cards win over the column chrome they sit on, and a card
    /// below its column's bottom edge is still a card.
    pub fn hit_test(&self, point: Point) -> KanbanTarget {
        if !self.rect.contains(point) {
            return KanbanTarget::Outside;
        }
        for column in &self.columns {
            if let Some(card) = column.cards.iter().find(|card| card.rect.contains(point)) {
                return KanbanTarget::Card { column: column.id, card: card.id };
            }
        }
        if self.title.contains(point) {
            return KanbanTarget::Title;
        }
        for column in &self.columns {
            if column.header.contains(point) {
                return KanbanTarget::ColumnHeader(column.id);
            }
            if column.rect.contains(point) {
                return KanbanTarget::ColumnBody(column.id);
            }
        }
        KanbanTarget::Board
    }

    /// Where a card being dragged would land if it were dropped at `point`.
    ///
    /// `dragged` names the card in flight, when there is one. It is excluded from the
    /// stack the insertion point is measured against, which is what makes the preview
    /// exact: the card is treated as already lifted out, the cards below it close the
    /// gap, and `preview` is the rectangle the card will actually occupy once
    /// [`Kanban::move_card`] is called with `index`. Passing `None` previews a card
    /// arriving from elsewhere, using the default card height.
    ///
    /// The column is chosen by horizontal position alone, so a drop above the first
    /// card or below the last still belongs to the column it is over. A point in the
    /// gap between two columns snaps to the nearer one rather than returning nothing,
    /// because a preview that blinks out every time the pointer crosses a 12px gutter
    /// is worse than one that commits to an answer.
    pub fn drop_target(&self, point: Point, dragged: Option<CardId>) -> Option<KanbanDrop> {
        if !self.rect.contains(point) || self.columns.is_empty() {
            return None;
        }
        let distance = |column: &ColumnLayout| horizontal_distance(column.rect, point.x);
        let column = self.columns.iter().min_by(|a, b| distance(a).total_cmp(&distance(b)))?;

        let height = dragged
            .and_then(|id| self.card(id))
            .map_or(self.metrics.card_height, |card| card.rect.height());

        // Walk the stack as it would be with the dragged card lifted out, stopping at
        // the first slot whose upper half the pointer is in.
        let mut top = column.body.top();
        let mut index = 0;
        for card in column.cards.iter().filter(|card| Some(card.id) != dragged) {
            if point.y < top + card.rect.height() * 0.5 {
                break;
            }
            top += card.rect.height() + self.metrics.card_gap;
            index += 1;
        }

        Some(KanbanDrop {
            column: column.id,
            index,
            preview: Rect::new(column.body.left(), top, column.body.width(), height),
        })
    }
}

/// How far `x` is from a rect horizontally; zero when inside it.
fn horizontal_distance(rect: Rect, x: f64) -> f64 {
    if x < rect.left() {
        rect.left() - x
    } else if x >= rect.right() {
        x - rect.right()
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Slot;

    const AREA: Rect =
        Rect { origin: Point { x: 0.0, y: 0.0 }, size: Size { width: 900.0, height: 600.0 } };

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

    #[test]
    fn columns_share_the_width_and_tile_without_gaps_of_their_own() {
        let (board, columns) = board();
        let m = board.metrics();
        let layout = board.layout(AREA);
        assert_eq!(layout.title, Rect::new(0.0, 0.0, 900.0, m.title_height));
        assert_eq!(layout.columns.len(), 3);

        let widths: Vec<f64> = layout.columns.iter().map(|c| c.rect.width()).collect();
        assert!(widths.windows(2).all(|w| (w[0] - w[1]).abs() < 1e-9), "{widths:?}");
        let total: f64 = widths.iter().sum::<f64>() + 2.0 * m.column_gap + 2.0 * m.padding;
        assert!((total - AREA.width()).abs() < 1e-9, "the columns fill the area exactly");

        // Adjacent columns are exactly one gutter apart.
        for pair in layout.columns.windows(2) {
            assert!((pair[1].rect.left() - pair[0].rect.right() - m.column_gap).abs() < 1e-9);
        }
        assert_eq!(layout.column(columns[0]).unwrap().rect.left(), m.padding);
        assert!(!layout.overflows());
    }

    #[test]
    fn cards_stack_down_their_column_in_order() {
        let (board, columns) = board();
        let m = board.metrics();
        let layout = board.layout(AREA);
        let column = layout.column(columns[0]).unwrap();
        assert_eq!(column.cards[0].rect.top(), column.body.top());
        for pair in column.cards.windows(2) {
            assert!((pair[1].rect.top() - pair[0].rect.bottom() - m.card_gap).abs() < 1e-9);
            assert_eq!(pair[0].rect.height(), m.card_height);
            assert_eq!(pair[0].rect.width(), column.body.width());
        }
        // The order on screen is the order in the model.
        let ids: Vec<CardId> = column.cards.iter().map(|c| c.id).collect();
        assert_eq!(ids, board.column(columns[0]).unwrap().card_ids());
    }

    #[test]
    fn a_measured_height_is_used_and_pushes_the_cards_below_it_down() {
        let (mut board, columns) = board();
        let first = board.column(columns[0]).unwrap().cards()[0].id();
        board.set_card_height(first, Some(140.0)).unwrap();
        let layout = board.layout(AREA);
        let column = layout.column(columns[0]).unwrap();
        assert_eq!(column.cards[0].rect.height(), 140.0);
        assert_eq!(column.cards[1].rect.top(), column.body.top() + 140.0 + board.metrics().card_gap);
    }

    /// Too many columns for the area: they keep their minimum width and run off the
    /// right-hand edge, and the layout says so.
    #[test]
    fn a_crowded_board_overflows_rather_than_squeezing_its_columns() {
        let mut board = Kanban::new("wide");
        for n in 0..8 {
            board.add_column(format!("c{n}"));
        }
        let layout = board.layout(Rect::new(0.0, 0.0, 600.0, 400.0));
        let m = board.metrics();
        assert!(layout.columns.iter().all(|c| c.rect.width() == m.min_column_width));
        assert!(layout.content.width > 600.0);
        assert!(layout.overflows());
        assert!(layout.columns.last().unwrap().rect.right() > 600.0);
    }

    #[test]
    fn a_column_with_more_cards_than_fit_reports_the_height_it_needs() {
        let mut board = Kanban::new("tall");
        let column = board.add_column("only");
        for n in 0..20 {
            board.add_card(column, format!("card {n}")).unwrap();
        }
        let area = Rect::new(0.0, 0.0, 400.0, 300.0);
        let layout = board.layout(area);
        let laid = layout.column(column).unwrap();
        assert!(laid.content_height > laid.rect.height());
        assert!(layout.overflows());

        // The overflow is laid out past the bottom edge rather than being dropped…
        let spilled = laid.cards.last().unwrap();
        assert!(spilled.rect.top() > area.bottom());
        // …but hit-testing only answers for the area it was given.
        assert_eq!(layout.hit_test(spilled.rect.centre()), KanbanTarget::Outside);
        assert_eq!(layout.drop_target(spilled.rect.centre(), None), None);
        // A card hanging out of the bottom of its column, but still inside the area,
        // is still a card.
        let overhanging = laid
            .cards
            .iter()
            .find(|card| card.rect.bottom() > laid.body.bottom() && card.rect.centre().y < area.bottom())
            .expect("the stack runs past the column body");
        assert_eq!(
            layout.hit_test(overhanging.rect.centre()),
            KanbanTarget::Card { column, card: overhanging.id }
        );
    }

    #[test]
    fn an_empty_board_still_lays_out_its_chrome() {
        let board = Kanban::new("empty");
        let layout = board.layout(AREA);
        assert!(layout.columns.is_empty());
        assert_eq!(layout.title.height(), board.metrics().title_height);
        assert_eq!(layout.hit_test(Point::new(450.0, 20.0)), KanbanTarget::Title);
        assert_eq!(layout.drop_target(Point::new(450.0, 300.0), None), None);
    }

    #[test]
    fn hit_testing_finds_the_card_the_column_and_the_chrome() {
        let (board, columns) = board();
        let layout = board.layout(AREA);
        let column = layout.column(columns[1]).unwrap();
        let card = column.cards[1];

        assert_eq!(
            layout.hit_test(card.rect.centre()),
            KanbanTarget::Card { column: columns[1], card: card.id }
        );
        assert_eq!(layout.hit_test(column.header.centre()), KanbanTarget::ColumnHeader(columns[1]));
        assert_eq!(
            layout.hit_test(Point::new(column.body.centre().x, column.rect.bottom() - 1.0)),
            KanbanTarget::ColumnBody(columns[1])
        );
        // The gutter between two columns belongs to neither.
        let gutter = Point::new(column.rect.right() + 1.0, column.rect.centre().y);
        assert_eq!(layout.hit_test(gutter), KanbanTarget::Board);
        assert_eq!(layout.hit_test(Point::new(-5.0, 300.0)), KanbanTarget::Outside);
        assert_eq!(layout.hit_test(Point::new(450.0, 20.0)), KanbanTarget::Title);
    }

    /// The contract [`KanbanDrop`] claims: what the preview draws is where the card
    /// ends up. Checked for a drop into every slot of every column, from every
    /// column, which is the whole cross product a drag can produce.
    #[test]
    fn every_previewed_drop_lands_exactly_where_it_was_drawn() {
        let (board, columns) = board();
        let layout = board.layout(AREA);

        for &source in &columns {
            let dragged = board.column(source).unwrap().cards()[1].id();
            for &target in &columns {
                let column = layout.column(target).unwrap();
                // Sample the whole height of the column, hitting every slot boundary.
                for step in 0..12 {
                    let y = column.body.top() + step as f64 * 20.0;
                    let point = Point::new(column.body.centre().x, y);
                    let drop = layout.drop_target(point, Some(dragged)).expect("inside the board");
                    assert_eq!(drop.column, target);

                    let mut moved = board.clone();
                    moved.move_card(dragged, drop.column, Slot::Index(drop.index)).unwrap();
                    let after = moved.layout(AREA);
                    assert_eq!(
                        after.card(dragged).unwrap().rect,
                        drop.preview,
                        "dropping {dragged} from {source} into {target} at y={y}"
                    );
                }
            }
        }
    }

    /// The dragged card is treated as already lifted, so the cards below it close up
    /// and the index counts the list the card is landing in — one shorter than the
    /// list on screen, when the card came from this column.
    #[test]
    fn a_drop_index_ignores_the_card_in_flight() {
        let (board, columns) = board();
        let layout = board.layout(AREA);
        let column = layout.column(columns[0]).unwrap();
        let first = column.cards[0].id;
        let below = Point::new(column.body.centre().x, column.cards[2].rect.bottom() + 4.0);

        let gap = board.metrics().card_gap;
        let end_of_the_stack = column.cards[2].rect.bottom() + gap;

        let lifted = layout.drop_target(below, Some(first)).unwrap();
        assert_eq!(lifted.index, 2, "the end of a column of three that one has left");
        // With the first card gone the stack closes up by its height plus a gap, and
        // the slot at the end moves up with it.
        assert_eq!(lifted.preview.top(), end_of_the_stack - column.cards[0].rect.height() - gap);

        let arriving = layout.drop_target(below, None).unwrap();
        assert_eq!(arriving.index, 3, "the end of a column of three that keeps all of them");
        assert_eq!(arriving.preview.top(), end_of_the_stack);
    }

    #[test]
    fn a_drop_in_the_gutter_snaps_to_the_nearer_column() {
        let (board, columns) = board();
        let layout = board.layout(AREA);
        let left = layout.column(columns[0]).unwrap();
        let m = board.metrics();

        let just_right = Point::new(left.rect.right() + 1.0, left.body.centre().y);
        assert_eq!(layout.drop_target(just_right, None).unwrap().column, columns[0]);

        let just_left_of_next = Point::new(left.rect.right() + m.column_gap - 1.0, left.body.centre().y);
        assert_eq!(layout.drop_target(just_left_of_next, None).unwrap().column, columns[1]);
    }

    #[test]
    fn a_drop_below_the_last_card_lands_at_the_end() {
        let (board, columns) = board();
        let layout = board.layout(AREA);
        let column = layout.column(columns[0]).unwrap();
        let point = Point::new(column.body.centre().x, column.rect.bottom() - 1.0);
        let drop = layout.drop_target(point, None).unwrap();
        assert_eq!(drop.index, 3);
        let last = column.cards.last().unwrap().rect;
        assert_eq!(drop.preview.top(), last.bottom() + board.metrics().card_gap);
    }

    #[test]
    fn a_zero_sized_area_produces_zero_sized_chrome_rather_than_nonsense() {
        let (board, _) = board();
        let layout = board.layout(Rect::ZERO);
        assert!(layout.title.is_empty());
        assert!(layout.columns.iter().all(|c| c.body.height() == 0.0));
        assert!(layout.columns.iter().all(|c| c.rect.width() == board.metrics().min_column_width));
        assert!(layout.overflows());
    }
}
