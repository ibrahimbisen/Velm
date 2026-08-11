//! Turning a story map into rectangles, and turning a point back into a cell.
//!
//! Two things make this more than a nested loop over a grid.
//!
//! **A release row is as tall as its fullest cell.** Row height is content-driven, so
//! adding one story to one cell moves every row below it. That is correct — a story
//! map is read as a set of horizontal slices, and a cell that clipped its contents
//! would hide work from the release it belongs to — but it means the grid has to be
//! measured before any rectangle can be placed, which is why the row heights are
//! computed in their own pass.
//!
//! **A drag is laid out with the card already gone.** [`StoryMap::layout_lifting`]
//! takes the story in flight out of the measurement, so the row it came from has
//! already shrunk while the pointer is still moving. Without that, a drop preview in
//! a row below the source would be drawn one row-shrink too low and the card would
//! visibly jump on release. The kanban needs no such thing: its columns do not affect
//! each other's geometry, so lifting a card there is local to one column and
//! [`KanbanLayout::drop_target`](crate::kanban::KanbanLayout::drop_target) can do it
//! by itself.

use std::collections::HashMap;

use crate::geometry::{Point, Rect, Size};
use crate::id::{ActivityId, ReleaseId, StoryId};
use crate::metrics::StoryMapMetrics;
use crate::story_map::{CellRef, Story, StoryMap};

/// Absolute rectangles for a whole map.
#[derive(Debug, Clone, PartialEq)]
pub struct StoryMapLayout {
    /// The area the map was laid out in.
    pub rect: Rect,
    /// The container's title strip.
    pub title: Rect,
    /// Where the release rail meets the activity headers — top left, above the rail.
    pub corner: Rect,
    /// The grid itself: cells only, no headers.
    pub body: Rect,
    pub activities: Vec<ActivityLayout>,
    pub releases: Vec<ReleaseLayout>,
    /// Every position in the grid, including the empty ones — an empty cell is still
    /// somewhere a card can be dropped.
    pub cells: Vec<CellLayout>,
    /// What the map needs. Larger than `rect` on either axis when it overflows.
    pub content: Size,
    metrics: StoryMapMetrics,
    lifted: Option<Lifted>,
}

/// The story left out of this layout because it is being dragged, and how tall it
/// was, so a drop preview can still size itself.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Lifted {
    story: StoryId,
    height: f64,
}

/// One backbone column's header.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ActivityLayout {
    pub id: ActivityId,
    pub header: Rect,
}

/// One release row's header, in the rail down the left.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ReleaseLayout {
    pub id: ReleaseId,
    pub header: Rect,
}

/// One cell of the grid.
#[derive(Debug, Clone, PartialEq)]
pub struct CellLayout {
    pub at: CellRef,
    pub rect: Rect,
    pub stories: Vec<StoryLayout>,
    /// The height this cell's stories need. The row is as tall as the largest of
    /// these, so a cell with slack has `content_height < rect.height()`.
    pub content_height: f64,
}

/// One story card's rectangle.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StoryLayout {
    pub id: StoryId,
    pub rect: Rect,
}

/// What is under a point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoryMapTarget {
    Outside,
    Title,
    /// The empty square above the rail and left of the backbone.
    Corner,
    /// Inside the map but on none of its parts — padding, or a gutter.
    Chrome,
    ActivityHeader(ActivityId),
    ReleaseHeader(ReleaseId),
    /// A cell, but not one of its stories.
    Cell(CellRef),
    Story { cell: CellRef, story: StoryId },
}

/// Where a dragged story would land.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StoryMapDrop {
    pub cell: CellRef,
    pub index: usize,
    /// The slot that would open, in absolute coordinates.
    pub preview: Rect,
}

impl StoryMap {
    /// Absolute rectangles for the container chrome and every child, in `area`.
    pub fn layout(&self, area: Rect) -> StoryMapLayout {
        self.layout_lifting(area, None)
    }

    /// As [`StoryMap::layout`], with one story left out because it is being dragged.
    ///
    /// The lifted story is excluded from its cell *and* from the measurement of its
    /// row, so the map is already in the shape it will have when the drag ends. See
    /// the module docs for why this belongs at layout time here and not in the
    /// kanban.
    pub fn layout_lifting(&self, area: Rect, lifted: Option<StoryId>) -> StoryMapLayout {
        let m = self.metrics();
        let (title, below) = area.split_top(m.title_height);
        let inner = below.inset_by(m.padding);
        let (top_strip, rows_area) = inner.split_top(m.header_height);
        let (corner, headers) = top_strip.split_left(m.rail_width);
        let (rail, grid) = rows_area.split_left(m.rail_width);

        let activities = self.activities();
        let releases = self.releases();
        let columns = activities.len();
        let column_width = if columns == 0 {
            0.0
        } else {
            let gaps = m.column_gap * (columns - 1) as f64;
            ((grid.width() - gaps) / columns as f64).max(m.min_column_width)
        };
        let column_left = |index: usize| grid.left() + index as f64 * (column_width + m.column_gap);

        // Measure before placing: a row is as tall as its fullest cell, so no cell
        // rectangle can be written until every cell in that row has been sized.
        let filled: HashMap<CellRef, &[Story]> =
            self.cells().iter().map(|cell| (cell.at(), cell.stories())).collect();
        let content_height = |at: CellRef| {
            filled.get(&at).map_or(0.0, |stories| stack_height(stories, lifted, &m))
                + 2.0 * m.cell_padding
        };
        let row_heights: Vec<f64> = releases
            .iter()
            .map(|release| {
                activities
                    .iter()
                    .map(|activity| content_height(CellRef::new(activity.id(), release.id())))
                    .fold(m.min_row_height, f64::max)
            })
            .collect();

        let mut activity_layouts = Vec::with_capacity(columns);
        for (index, activity) in activities.iter().enumerate() {
            activity_layouts.push(ActivityLayout {
                id: activity.id(),
                header: Rect::new(column_left(index), headers.top(), column_width, headers.height()),
            });
        }

        let mut release_layouts = Vec::with_capacity(releases.len());
        let mut cells = Vec::with_capacity(columns * releases.len());
        let mut top = grid.top();
        for (row, release) in releases.iter().enumerate() {
            let row_height = row_heights[row];
            release_layouts.push(ReleaseLayout {
                id: release.id(),
                header: Rect::new(rail.left(), top, rail.width(), row_height),
            });
            for (column, activity) in activities.iter().enumerate() {
                let at = CellRef::new(activity.id(), release.id());
                let rect = Rect::new(column_left(column), top, column_width, row_height);
                let cell_inner = rect.inset_by(m.cell_padding);
                let mut stories = Vec::new();
                let mut y = cell_inner.top();
                for story in filled.get(&at).copied().unwrap_or_default() {
                    if Some(story.id()) == lifted {
                        continue;
                    }
                    let height = story_height(story, &m);
                    stories.push(StoryLayout {
                        id: story.id(),
                        rect: Rect::new(cell_inner.left(), y, cell_inner.width(), height),
                    });
                    y += height + m.card_gap;
                }
                cells.push(CellLayout { at, rect, stories, content_height: content_height(at) });
            }
            top += row_height + m.row_gap;
        }

        let content = Size::new(
            2.0 * m.padding
                + m.rail_width
                + columns as f64 * column_width
                + columns.saturating_sub(1) as f64 * m.column_gap,
            m.title_height
                + 2.0 * m.padding
                + m.header_height
                + row_heights.iter().sum::<f64>()
                + releases.len().saturating_sub(1) as f64 * m.row_gap,
        );

        let lifted = lifted.and_then(|id| {
            self.story(id).map(|story| Lifted { story: id, height: story_height(story, &m) })
        });

        StoryMapLayout {
            rect: area,
            title,
            corner,
            body: grid,
            activities: activity_layouts,
            releases: release_layouts,
            cells,
            content,
            metrics: m,
            lifted,
        }
    }
}

fn story_height(story: &Story, metrics: &StoryMapMetrics) -> f64 {
    story.height().filter(|h| *h > 0.0).unwrap_or(metrics.card_height)
}

/// The height a cell's stories occupy, gaps included, ignoring the one in flight.
fn stack_height(stories: &[Story], lifted: Option<StoryId>, metrics: &StoryMapMetrics) -> f64 {
    let counted = stories.iter().filter(|story| Some(story.id()) != lifted);
    let count = counted.clone().count();
    let heights: f64 = counted.map(|story| story_height(story, metrics)).sum();
    heights + metrics.card_gap * count.saturating_sub(1) as f64
}

impl StoryMapLayout {
    pub fn cell(&self, at: CellRef) -> Option<&CellLayout> {
        self.cells.iter().find(|cell| cell.at == at)
    }

    pub fn story(&self, id: StoryId) -> Option<&StoryLayout> {
        self.cells.iter().flat_map(|cell| &cell.stories).find(|story| story.id == id)
    }

    pub fn overflows(&self) -> bool {
        self.content.width > self.rect.width() || self.content.height > self.rect.height()
    }

    /// What is under `point`.
    pub fn hit_test(&self, point: Point) -> StoryMapTarget {
        if !self.rect.contains(point) {
            return StoryMapTarget::Outside;
        }
        if self.title.contains(point) {
            return StoryMapTarget::Title;
        }
        if self.corner.contains(point) {
            return StoryMapTarget::Corner;
        }
        if let Some(activity) = self.activities.iter().find(|a| a.header.contains(point)) {
            return StoryMapTarget::ActivityHeader(activity.id);
        }
        if let Some(release) = self.releases.iter().find(|r| r.header.contains(point)) {
            return StoryMapTarget::ReleaseHeader(release.id);
        }
        for cell in &self.cells {
            if let Some(story) = cell.stories.iter().find(|story| story.rect.contains(point)) {
                return StoryMapTarget::Story { cell: cell.at, story: story.id };
            }
            if cell.rect.contains(point) {
                return StoryMapTarget::Cell(cell.at);
            }
        }
        StoryMapTarget::Chrome
    }

    /// Where a story being dragged would land if it were dropped at `point`.
    ///
    /// The index is a [`Slot::Index`](crate::Slot::Index) in the destination cell, and
    /// `preview` is the rectangle the story will occupy — exactly, provided this
    /// layout was produced by [`StoryMap::layout_lifting`] with the same story. If it
    /// was not, the index is still right but the preview can be a row-shrink out of
    /// date, because the row the story came from has not yet closed up.
    ///
    /// A point in a gutter snaps to the nearest cell rather than returning nothing,
    /// so a preview does not blink out while the pointer crosses between cells.
    pub fn drop_target(&self, point: Point, dragged: Option<StoryId>) -> Option<StoryMapDrop> {
        if !self.rect.contains(point) || self.cells.is_empty() {
            return None;
        }
        let cell = self
            .cells
            .iter()
            .min_by(|a, b| distance(a.rect, point).total_cmp(&distance(b.rect, point)))?;

        let height = match self.lifted {
            Some(lifted) if Some(lifted.story) == dragged => lifted.height,
            _ => dragged
                .and_then(|id| self.story(id))
                .map_or(self.metrics.card_height, |story| story.rect.height()),
        };

        let inner = cell.rect.inset_by(self.metrics.cell_padding);
        let mut top = inner.top();
        let mut index = 0;
        for story in cell.stories.iter().filter(|story| Some(story.id) != dragged) {
            if point.y < top + story.rect.height() * 0.5 {
                break;
            }
            top += story.rect.height() + self.metrics.card_gap;
            index += 1;
        }

        Some(StoryMapDrop {
            cell: cell.at,
            index,
            preview: Rect::new(inner.left(), top, inner.width(), height),
        })
    }
}

/// How far a point is from a rect, as the sum of its axis distances. Zero inside it.
///
/// Manhattan rather than Euclidean because it needs no square root and orders the
/// candidates identically for the axis-aligned grid this is used on.
fn distance(rect: Rect, point: Point) -> f64 {
    let dx = (rect.left() - point.x).max(point.x - rect.right()).max(0.0);
    let dy = (rect.top() - point.y).max(point.y - rect.bottom()).max(0.0);
    dx + dy
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Slot;

    const AREA: Rect =
        Rect { origin: Point { x: 0.0, y: 0.0 }, size: Size { width: 1000.0, height: 700.0 } };

    fn map() -> (StoryMap, Vec<ActivityId>, Vec<ReleaseId>) {
        let mut map = StoryMap::new("Onboarding");
        let activities: Vec<ActivityId> =
            ["Browse", "Choose", "Check out"].into_iter().map(|t| map.add_activity(t)).collect();
        let releases: Vec<ReleaseId> =
            ["MVP", "Beta"].into_iter().map(|t| map.add_release(t)).collect();
        for (column, &activity) in activities.iter().enumerate() {
            for (row, &release) in releases.iter().enumerate() {
                let at = CellRef::new(activity, release);
                map.add_story(at, format!("s{column}-{row}")).unwrap();
            }
        }
        (map, activities, releases)
    }

    #[test]
    fn the_grid_sits_below_the_headers_and_right_of_the_rail() {
        let (map, activities, releases) = map();
        let m = map.metrics();
        let layout = map.layout(AREA);

        assert_eq!(layout.corner.width(), m.rail_width);
        assert_eq!(layout.corner.height(), m.header_height);
        assert_eq!(layout.body.left(), m.padding + m.rail_width);
        assert_eq!(layout.body.top(), m.title_height + m.padding + m.header_height);

        let first = layout.activities[0];
        assert_eq!(first.id, activities[0]);
        assert_eq!(first.header.left(), layout.body.left());
        assert_eq!(first.header.bottom(), layout.body.top());

        let row = layout.releases[0];
        assert_eq!(row.id, releases[0]);
        assert_eq!(row.header.left(), layout.corner.left());
        assert_eq!(row.header.top(), layout.body.top());

        // A cell sits under its activity and beside its release.
        let cell = layout.cell(CellRef::new(activities[0], releases[0])).unwrap();
        assert_eq!(cell.rect.left(), first.header.left());
        assert_eq!(cell.rect.width(), first.header.width());
        assert_eq!(cell.rect.top(), row.header.top());
        assert_eq!(cell.rect.height(), row.header.height());
        assert_eq!(layout.cells.len(), 6, "every position, not only the filled ones");
    }

    #[test]
    fn a_row_is_as_tall_as_its_fullest_cell() {
        let (mut map, activities, releases) = map();
        let m = map.metrics();
        let crowded = CellRef::new(activities[1], releases[0]);
        for n in 0..4 {
            map.add_story(crowded, format!("extra {n}")).unwrap();
        }

        let layout = map.layout(AREA);
        let tall = layout.cell(crowded).unwrap();
        let expected = 5.0 * m.card_height + 4.0 * m.card_gap + 2.0 * m.cell_padding;
        assert_eq!(tall.content_height, expected);
        assert_eq!(tall.rect.height(), expected, "the row grew to fit it");

        // Its neighbours in the same row are the same height with room to spare.
        let quiet = layout.cell(CellRef::new(activities[0], releases[0])).unwrap();
        assert_eq!(quiet.rect.height(), expected);
        assert!(quiet.content_height < quiet.rect.height());

        // And the row below moved down by exactly the difference.
        let second_row = layout.cell(CellRef::new(activities[0], releases[1])).unwrap();
        assert_eq!(second_row.rect.top(), quiet.rect.bottom() + m.row_gap);
        // An empty row never collapses below its minimum.
        assert_eq!(second_row.rect.height(), m.min_row_height.max(m.card_height + 2.0 * m.cell_padding));
    }

    #[test]
    fn stories_stack_inside_their_cell_padding() {
        let (mut map, activities, releases) = map();
        let m = map.metrics();
        let at = CellRef::new(activities[0], releases[0]);
        map.add_story(at, "second").unwrap();
        let layout = map.layout(AREA);
        let cell = layout.cell(at).unwrap();

        assert_eq!(cell.stories.len(), 2);
        assert_eq!(cell.stories[0].rect.left(), cell.rect.left() + m.cell_padding);
        assert_eq!(cell.stories[0].rect.top(), cell.rect.top() + m.cell_padding);
        assert_eq!(cell.stories[0].rect.width(), cell.rect.width() - 2.0 * m.cell_padding);
        assert_eq!(cell.stories[1].rect.top(), cell.stories[0].rect.bottom() + m.card_gap);
    }

    #[test]
    fn lifting_a_story_takes_it_out_of_the_layout_and_shrinks_its_row() {
        let (mut map, activities, releases) = map();
        let at = CellRef::new(activities[0], releases[0]);
        let extra = map.add_story(at, "second").unwrap();
        map.set_story_height(extra, Some(200.0)).unwrap();

        let settled = map.layout(AREA);
        let dragging = map.layout_lifting(AREA, Some(extra));
        assert!(settled.cell(at).unwrap().rect.height() > dragging.cell(at).unwrap().rect.height());
        assert_eq!(dragging.story(extra), None, "the lifted story is not drawn in place");
        assert_eq!(dragging.cell(at).unwrap().stories.len(), 1);
        // The row below has moved up with it.
        let below = CellRef::new(activities[0], releases[1]);
        assert!(dragging.cell(below).unwrap().rect.top() < settled.cell(below).unwrap().rect.top());
    }

    #[test]
    fn hit_testing_finds_every_part_of_the_map() {
        let (map, activities, releases) = map();
        let layout = map.layout(AREA);
        let at = CellRef::new(activities[1], releases[1]);
        let cell = layout.cell(at).unwrap();
        let story = cell.stories[0];

        assert_eq!(layout.hit_test(story.rect.centre()), StoryMapTarget::Story { cell: at, story: story.id });
        assert_eq!(
            layout.hit_test(Point::new(cell.rect.centre().x, cell.rect.bottom() - 1.0)),
            StoryMapTarget::Cell(at)
        );
        assert_eq!(
            layout.hit_test(layout.activities[1].header.centre()),
            StoryMapTarget::ActivityHeader(activities[1])
        );
        assert_eq!(
            layout.hit_test(layout.releases[1].header.centre()),
            StoryMapTarget::ReleaseHeader(releases[1])
        );
        assert_eq!(layout.hit_test(layout.corner.centre()), StoryMapTarget::Corner);
        assert_eq!(layout.hit_test(layout.title.centre()), StoryMapTarget::Title);
        assert_eq!(layout.hit_test(Point::new(-1.0, -1.0)), StoryMapTarget::Outside);
        // The padding between the title and the header row is chrome.
        assert_eq!(
            layout.hit_test(Point::new(500.0, layout.title.bottom() + 1.0)),
            StoryMapTarget::Chrome
        );
    }

    /// The contract: a preview drawn during a drag is where the story ends up.
    /// Checked from every cell into every cell, at several heights.
    #[test]
    fn every_previewed_drop_lands_exactly_where_it_was_drawn() {
        let (mut map, activities, releases) = map();
        // Uneven heights, so a drop that changes a row's height is in the sample.
        let tall = CellRef::new(activities[0], releases[0]);
        let extra = map.add_story(tall, "tall").unwrap();
        map.set_story_height(extra, Some(120.0)).unwrap();

        for &source_activity in &activities {
            for &source_release in &releases {
                let source = CellRef::new(source_activity, source_release);
                let dragged = map.stories_in(source)[0].id();
                let layout = map.layout_lifting(AREA, Some(dragged));

                for cell in &layout.cells {
                    for step in 0..4 {
                        let point = Point::new(
                            cell.rect.centre().x,
                            cell.rect.top() + step as f64 * 30.0 + 2.0,
                        );
                        let Some(drop) = layout.drop_target(point, Some(dragged)) else {
                            continue;
                        };
                        let mut moved = map.clone();
                        moved.move_story(dragged, drop.cell, Slot::Index(drop.index)).unwrap();
                        assert_eq!(
                            moved.layout(AREA).story(dragged).unwrap().rect,
                            drop.preview,
                            "dropping {dragged} from {source:?} into {:?}",
                            drop.cell
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn a_drop_below_the_last_story_lands_at_the_end_of_the_cell() {
        let (map, activities, releases) = map();
        let layout = map.layout(AREA);
        let at = CellRef::new(activities[0], releases[0]);
        let cell = layout.cell(at).unwrap();
        let point = Point::new(cell.rect.centre().x, cell.rect.bottom() - 1.0);
        let drop = layout.drop_target(point, None).unwrap();
        assert_eq!(drop.cell, at);
        assert_eq!(drop.index, 1);
        assert_eq!(drop.preview.top(), cell.stories[0].rect.bottom() + map.metrics().card_gap);
    }

    #[test]
    fn an_empty_map_lays_out_its_chrome_and_offers_no_drop() {
        let map = StoryMap::new("empty");
        let layout = map.layout(AREA);
        assert!(layout.cells.is_empty());
        assert!(layout.activities.is_empty());
        assert_eq!(layout.drop_target(Point::new(500.0, 400.0), None), None);
        assert_eq!(layout.hit_test(Point::new(500.0, 400.0)), StoryMapTarget::Chrome);
        assert_eq!(layout.hit_test(layout.title.centre()), StoryMapTarget::Title);
    }

    #[test]
    fn a_map_wider_than_its_area_overflows_rather_than_squeezing() {
        let mut map = StoryMap::new("wide");
        for n in 0..10 {
            map.add_activity(format!("a{n}"));
        }
        map.add_release("r");
        let layout = map.layout(Rect::new(0.0, 0.0, 800.0, 400.0));
        let m = map.metrics();
        assert!(layout.activities.iter().all(|a| a.header.width() == m.min_column_width));
        assert!(layout.overflows());
        assert!(layout.content.width > 800.0);
    }
}
