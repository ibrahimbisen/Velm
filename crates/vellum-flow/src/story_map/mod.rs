//! User story mapping: activities across the top, releases down the side, stories
//! in the cells.
//!
//! ```
//! use vellum_flow::{CellRef, Slot, StoryMap};
//!
//! let mut map = StoryMap::new("Onboarding");
//! let browse = map.add_activity("Browse");
//! let checkout = map.add_activity("Check out");
//! let mvp = map.add_release("MVP");
//! let later = map.add_release("Later");
//!
//! let search = map.add_story(CellRef::new(browse, mvp), "Search").unwrap();
//! map.add_story(CellRef::new(browse, mvp), "Filter").unwrap();
//! map.add_story(CellRef::new(checkout, later), "Apple Pay").unwrap();
//!
//! // A release inserted between two others moves no story at all.
//! let beta = map.insert_release(1, "Beta");
//! assert_eq!(map.locate(search), Some((CellRef::new(browse, mvp), 0)));
//!
//! // Deleting an activity hands its stories back rather than dropping them.
//! map.move_story(search, CellRef::new(checkout, beta), Slot::Top).unwrap();
//! let removed = map.remove_activity(browse).unwrap();
//! assert_eq!(removed.story_count(), 1); // "Filter" was still in MVP
//! assert_eq!(map.story_count(), 2);
//! ```
//!
//! # Cells are keyed by id, which is the whole trick
//!
//! A cell is addressed by a [`CellRef`] — an [`ActivityId`] and a [`ReleaseId`] —
//! never by a row and column number. So inserting an activity in the middle of the
//! backbone, or deleting a release, **rewrites no cell key**: the stories carry on
//! naming the same activity and release they always did, and the grid is just drawn
//! with one more column or one fewer row.
//!
//! Numbered cells would make every structural edit a migration over the whole map,
//! and the failure mode of getting that migration wrong is stories silently landing
//! in the wrong release — which is not a crash, not an error, and not something
//! anyone notices until the plan is wrong.
//!
//! # Empty cells are not stored
//!
//! The grid is the cross product of the activities and the releases; a [`Cell`]
//! exists only once something is put in it, and stops existing when the last story
//! leaves. A 30 × 20 map with a dozen stories in it is a dozen cells, not six
//! hundred. Layout still emits a rectangle for every position — an empty cell is
//! still a drop target — but the model does not carry six hundred empty vectors
//! through every save.

mod layout;

pub use layout::{
    ActivityLayout, CellLayout, ReleaseLayout, StoryLayout, StoryMapDrop, StoryMapLayout,
    StoryMapTarget,
};

use serde::{Deserialize, Serialize};

use crate::error::FlowError;
use crate::id::{ActivityId, IdSource, ReleaseId, StoryId};
use crate::metrics::StoryMapMetrics;
use crate::rank::Rank;
use crate::slot::{Ranked, Slot, rank_at};

/// A two-axis grid of stories. The container itself.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoryMap {
    title: String,
    activities: Vec<Activity>,
    releases: Vec<Release>,
    cells: Vec<Cell>,
    metrics: StoryMapMetrics,
    activity_ids: IdSource,
    release_ids: IdSource,
    story_ids: IdSource,
}

/// One column of the backbone: a step in the user's journey.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Activity {
    id: ActivityId,
    title: String,
}

/// One row: a release, a sprint, a slice — whatever the horizontal bands mean on
/// this map.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Release {
    id: ReleaseId,
    title: String,
}

/// Which cell: the activity column and the release row it sits in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct CellRef {
    pub activity: ActivityId,
    pub release: ReleaseId,
}

impl CellRef {
    pub const fn new(activity: ActivityId, release: ReleaseId) -> Self {
        Self { activity, release }
    }
}

/// One non-empty cell and its stories, in order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Cell {
    at: CellRef,
    stories: Vec<Story>,
}

/// One story card.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Story {
    id: StoryId,
    label: String,
    rank: Rank,
    height: Option<f64>,
}

impl Ranked for Story {
    fn rank(&self) -> &Rank {
        &self.rank
    }
}

/// What [`StoryMap::move_story`] wrote: one story, its cell and its rank.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoryMove {
    pub story: StoryId,
    pub from: CellRef,
    pub to: CellRef,
    pub rank: Rank,
}

/// An activity that has been deleted, with everything that was underneath it.
///
/// Returned rather than discarded: deleting a backbone column with a release plan
/// under it is the destructive edit on this widget, and the caller needs the content
/// both to undo it and to ask the question first.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RemovedActivity {
    pub activity: Activity,
    /// The cells that were in that column, each still naming the release it was in.
    pub cells: Vec<Cell>,
}

/// A release that has been deleted, with the row's contents.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RemovedRelease {
    pub release: Release,
    pub cells: Vec<Cell>,
}

impl RemovedActivity {
    pub fn story_count(&self) -> usize {
        self.cells.iter().map(|cell| cell.stories.len()).sum()
    }
}

impl RemovedRelease {
    pub fn story_count(&self) -> usize {
        self.cells.iter().map(|cell| cell.stories.len()).sum()
    }
}

impl Activity {
    pub fn id(&self) -> ActivityId {
        self.id
    }

    pub fn title(&self) -> &str {
        &self.title
    }
}

impl Release {
    pub fn id(&self) -> ReleaseId {
        self.id
    }

    pub fn title(&self) -> &str {
        &self.title
    }
}

impl Cell {
    pub fn at(&self) -> CellRef {
        self.at
    }

    pub fn stories(&self) -> &[Story] {
        &self.stories
    }
}

impl Story {
    pub fn id(&self) -> StoryId {
        self.id
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn rank(&self) -> &Rank {
        &self.rank
    }

    /// The measured height, or `None` for
    /// [`StoryMapMetrics::card_height`](crate::StoryMapMetrics::card_height).
    pub fn height(&self) -> Option<f64> {
        self.height
    }
}

impl StoryMap {
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            activities: Vec::new(),
            releases: Vec::new(),
            cells: Vec::new(),
            metrics: StoryMapMetrics::default(),
            activity_ids: IdSource::default(),
            release_ids: IdSource::default(),
            story_ids: IdSource::default(),
        }
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    pub fn set_title(&mut self, title: impl Into<String>) {
        self.title = title.into();
    }

    pub fn metrics(&self) -> StoryMapMetrics {
        self.metrics
    }

    pub fn set_metrics(&mut self, metrics: StoryMapMetrics) {
        self.metrics = metrics;
    }

    /// The backbone, left to right.
    pub fn activities(&self) -> &[Activity] {
        &self.activities
    }

    /// The releases, top to bottom.
    pub fn releases(&self) -> &[Release] {
        &self.releases
    }

    /// Only the cells that hold something. See the module docs.
    pub fn cells(&self) -> &[Cell] {
        &self.cells
    }

    pub fn activity_index(&self, id: ActivityId) -> Option<usize> {
        self.activities.iter().position(|activity| activity.id == id)
    }

    pub fn release_index(&self, id: ReleaseId) -> Option<usize> {
        self.releases.iter().position(|release| release.id == id)
    }

    pub fn story_count(&self) -> usize {
        self.cells.iter().map(|cell| cell.stories.len()).sum()
    }

    // ----- the backbone ---------------------------------------------------

    pub fn add_activity(&mut self, title: impl Into<String>) -> ActivityId {
        self.insert_activity(self.activities.len(), title)
    }

    /// Inserts an activity, clamping `index` to the end.
    ///
    /// No cell is read or written. Every story stays in the cell it names, which is
    /// the property the module docs are about.
    pub fn insert_activity(&mut self, index: usize, title: impl Into<String>) -> ActivityId {
        let id = ActivityId::from_raw(self.activity_ids.next());
        let index = index.min(self.activities.len());
        self.activities.insert(index, Activity { id, title: title.into() });
        id
    }

    /// Deletes an activity and returns the column's contents.
    pub fn remove_activity(&mut self, id: ActivityId) -> Result<RemovedActivity, FlowError> {
        let index = self.activity_index(id).ok_or(FlowError::NoSuchActivity(id))?;
        let activity = self.activities.remove(index);
        let cells = self.take_cells(|at| at.activity == id);
        Ok(RemovedActivity { activity, cells })
    }

    pub fn rename_activity(&mut self, id: ActivityId, title: impl Into<String>) -> Result<(), FlowError> {
        let index = self.activity_index(id).ok_or(FlowError::NoSuchActivity(id))?;
        self.activities[index].title = title.into();
        Ok(())
    }

    /// Reorders the backbone. Cells are keyed by id, so nothing moves but the
    /// columns themselves.
    pub fn move_activity(&mut self, id: ActivityId, to_index: usize) -> Result<(), FlowError> {
        let from = self.activity_index(id).ok_or(FlowError::NoSuchActivity(id))?;
        let to = to_index.min(self.activities.len() - 1);
        if from != to {
            let activity = self.activities.remove(from);
            self.activities.insert(to, activity);
        }
        Ok(())
    }

    // ----- releases -------------------------------------------------------

    pub fn add_release(&mut self, title: impl Into<String>) -> ReleaseId {
        self.insert_release(self.releases.len(), title)
    }

    /// Inserts a release row, clamping `index` to the end. As with
    /// [`StoryMap::insert_activity`], no cell is touched.
    pub fn insert_release(&mut self, index: usize, title: impl Into<String>) -> ReleaseId {
        let id = ReleaseId::from_raw(self.release_ids.next());
        let index = index.min(self.releases.len());
        self.releases.insert(index, Release { id, title: title.into() });
        id
    }

    /// Deletes a release and returns the row's contents.
    pub fn remove_release(&mut self, id: ReleaseId) -> Result<RemovedRelease, FlowError> {
        let index = self.release_index(id).ok_or(FlowError::NoSuchRelease(id))?;
        let release = self.releases.remove(index);
        let cells = self.take_cells(|at| at.release == id);
        Ok(RemovedRelease { release, cells })
    }

    pub fn rename_release(&mut self, id: ReleaseId, title: impl Into<String>) -> Result<(), FlowError> {
        let index = self.release_index(id).ok_or(FlowError::NoSuchRelease(id))?;
        self.releases[index].title = title.into();
        Ok(())
    }

    pub fn move_release(&mut self, id: ReleaseId, to_index: usize) -> Result<(), FlowError> {
        let from = self.release_index(id).ok_or(FlowError::NoSuchRelease(id))?;
        let to = to_index.min(self.releases.len() - 1);
        if from != to {
            let release = self.releases.remove(from);
            self.releases.insert(to, release);
        }
        Ok(())
    }

    // ----- stories --------------------------------------------------------

    pub fn cell(&self, at: CellRef) -> Option<&Cell> {
        self.cells.iter().find(|cell| cell.at == at)
    }

    /// The stories in a cell, in order. Empty for a cell nothing has been put in.
    pub fn stories_in(&self, at: CellRef) -> &[Story] {
        self.cell(at).map_or(&[], |cell| &cell.stories)
    }

    /// Appends a story to the bottom of a cell.
    pub fn add_story(&mut self, at: CellRef, label: impl Into<String>) -> Result<StoryId, FlowError> {
        self.insert_story(at, label, Slot::Bottom)
    }

    pub fn insert_story(
        &mut self,
        at: CellRef,
        label: impl Into<String>,
        slot: Slot,
    ) -> Result<StoryId, FlowError> {
        self.check_cell(at)?;
        let id = StoryId::from_raw(self.story_ids.next());
        let label = label.into();
        let cell = self.cell_mut(at);
        let index = slot.index_in(cell.stories.len());
        let rank = rank_at(&cell.stories, index);
        cell.stories.insert(index, Story { id, label, rank, height: None });
        Ok(id)
    }

    /// Moves a story to another cell, landing at `slot`.
    ///
    /// One write, exactly as in the kanban: the story's cell and rank. No other
    /// story in either cell changes.
    pub fn move_story(&mut self, story: StoryId, to: CellRef, slot: Slot) -> Result<StoryMove, FlowError> {
        let (cell_index, story_index) =
            self.locate_indices(story).ok_or(FlowError::NoSuchStory(story))?;
        self.check_cell(to)?;
        let from = self.cells[cell_index].at;

        let mut moved = self.cells[cell_index].stories.remove(story_index);
        self.prune_if_empty(cell_index);

        let destination = self.cell_mut(to);
        let index = slot.index_in(destination.stories.len());
        moved.rank = rank_at(&destination.stories, index);
        let rank = moved.rank.clone();
        destination.stories.insert(index, moved);
        Ok(StoryMove { story, from, to, rank })
    }

    pub fn remove_story(&mut self, story: StoryId) -> Result<Story, FlowError> {
        let (cell_index, story_index) =
            self.locate_indices(story).ok_or(FlowError::NoSuchStory(story))?;
        let removed = self.cells[cell_index].stories.remove(story_index);
        self.prune_if_empty(cell_index);
        Ok(removed)
    }

    pub fn story(&self, story: StoryId) -> Option<&Story> {
        self.cells.iter().flat_map(|cell| &cell.stories).find(|s| s.id == story)
    }

    /// Which cell a story is in, and where in it.
    pub fn locate(&self, story: StoryId) -> Option<(CellRef, usize)> {
        let (cell, index) = self.locate_indices(story)?;
        Some((self.cells[cell].at, index))
    }

    pub fn set_story_height(&mut self, story: StoryId, height: Option<f64>) -> Result<(), FlowError> {
        let (cell, index) = self.locate_indices(story).ok_or(FlowError::NoSuchStory(story))?;
        self.cells[cell].stories[index].height = height.map(|h| h.max(0.0));
        Ok(())
    }

    pub fn set_story_label(&mut self, story: StoryId, label: impl Into<String>) -> Result<(), FlowError> {
        let (cell, index) = self.locate_indices(story).ok_or(FlowError::NoSuchStory(story))?;
        self.cells[cell].stories[index].label = label.into();
        Ok(())
    }

    // ----- internals ------------------------------------------------------

    /// Both axes have to name something. Checked before any mutation so a bad cell
    /// reference cannot half-apply a move.
    fn check_cell(&self, at: CellRef) -> Result<(), FlowError> {
        if self.activity_index(at.activity).is_none() {
            return Err(FlowError::NoSuchActivity(at.activity));
        }
        if self.release_index(at.release).is_none() {
            return Err(FlowError::NoSuchRelease(at.release));
        }
        Ok(())
    }

    /// The cell at `at`, created empty if it does not exist yet. Only called once
    /// [`StoryMap::check_cell`] has passed.
    fn cell_mut(&mut self, at: CellRef) -> &mut Cell {
        match self.cells.iter().position(|cell| cell.at == at) {
            Some(index) => &mut self.cells[index],
            None => {
                self.cells.push(Cell { at, stories: Vec::new() });
                self.cells.last_mut().expect("just pushed")
            }
        }
    }

    fn prune_if_empty(&mut self, index: usize) {
        if self.cells[index].stories.is_empty() {
            self.cells.remove(index);
        }
    }

    fn take_cells(&mut self, mut matches: impl FnMut(CellRef) -> bool) -> Vec<Cell> {
        let mut taken = Vec::new();
        self.cells.retain(|cell| {
            if matches(cell.at) {
                taken.push(cell.clone());
                false
            } else {
                true
            }
        });
        taken
    }

    fn locate_indices(&self, story: StoryId) -> Option<(usize, usize)> {
        self.cells.iter().enumerate().find_map(|(cell, c)| {
            c.stories.iter().position(|existing| existing.id == story).map(|index| (cell, index))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 3 × 3 map with one story in every cell, labelled by its position, so an
    /// assertion about what survived an edit reads directly.
    fn map() -> (StoryMap, Vec<ActivityId>, Vec<ReleaseId>) {
        let mut map = StoryMap::new("Onboarding");
        let activities: Vec<ActivityId> =
            ["Browse", "Choose", "Check out"].into_iter().map(|t| map.add_activity(t)).collect();
        let releases: Vec<ReleaseId> =
            ["MVP", "Beta", "Later"].into_iter().map(|t| map.add_release(t)).collect();
        for (column, &activity) in activities.iter().enumerate() {
            for (row, &release) in releases.iter().enumerate() {
                let at = CellRef::new(activity, release);
                map.add_story(at, format!("s{column}-{row}")).unwrap();
                map.add_story(at, format!("s{column}-{row}b")).unwrap();
            }
        }
        (map, activities, releases)
    }

    /// Every story, as (cell, label, rank). The comparison used below to say
    /// "nothing moved" in the strongest available sense.
    fn census(map: &StoryMap) -> Vec<(CellRef, String, Rank)> {
        let mut rows: Vec<(CellRef, String, Rank)> = map
            .cells()
            .iter()
            .flat_map(|cell| {
                cell.stories().iter().map(|s| (cell.at(), s.label().to_owned(), s.rank().clone()))
            })
            .collect();
        rows.sort();
        rows
    }

    #[test]
    fn inserting_a_column_or_a_row_moves_no_story() {
        let (mut map, activities, releases) = map();
        let before = census(&map);

        map.insert_activity(1, "Compare");
        map.insert_release(2, "Sprint 3");

        assert_eq!(census(&map), before, "no story may change cell, order or rank");
        assert_eq!(map.activities().len(), 4);
        assert_eq!(map.releases().len(), 4);
        assert_eq!(map.story_count(), 18);
        // The new column and row are empty, and the old ones are where they were.
        assert_eq!(map.activity_index(activities[1]), Some(2));
        assert_eq!(map.release_index(releases[2]), Some(3));
    }

    /// The required case: delete a column that has cards in it.
    #[test]
    fn deleting_an_activity_returns_its_stories_and_leaves_the_rest_untouched() {
        let (mut map, activities, releases) = map();
        let doomed = activities[1];
        let survivors: Vec<(CellRef, String, Rank)> =
            census(&map).into_iter().filter(|(at, ..)| at.activity != doomed).collect();

        let removed = map.remove_activity(doomed).unwrap();

        assert_eq!(removed.activity.title(), "Choose");
        assert_eq!(removed.story_count(), 6, "two stories in each of three releases");
        assert_eq!(removed.cells.len(), 3);
        // Every returned cell still says which release it came from.
        for cell in &removed.cells {
            assert_eq!(cell.at().activity, doomed);
            assert!(releases.contains(&cell.at().release));
        }

        assert_eq!(map.story_count(), 12);
        assert_eq!(census(&map), survivors, "the other columns are byte-identical");
        assert_eq!(map.activities().len(), 2);
        // The deleted column's stories are gone from the map, handles and all.
        let orphan = removed.cells[0].stories()[0].id();
        assert_eq!(map.story(orphan), None);
        assert_eq!(map.remove_story(orphan), Err(FlowError::NoSuchStory(orphan)));
    }

    #[test]
    fn deleting_a_release_returns_the_row_and_leaves_the_rest_untouched() {
        let (mut map, _, releases) = map();
        let doomed = releases[0];
        let survivors: Vec<(CellRef, String, Rank)> =
            census(&map).into_iter().filter(|(at, ..)| at.release != doomed).collect();

        let removed = map.remove_release(doomed).unwrap();
        assert_eq!(removed.release.title(), "MVP");
        assert_eq!(removed.story_count(), 6);
        assert_eq!(map.story_count(), 12);
        assert_eq!(census(&map), survivors);
        assert_eq!(map.releases().len(), 2);
    }

    #[test]
    fn deleting_an_empty_row_takes_no_cells_with_it() {
        let (mut map, _, _) = map();
        let empty = map.add_release("Someday");
        let before = census(&map);
        let removed = map.remove_release(empty).unwrap();
        assert!(removed.cells.is_empty());
        assert_eq!(census(&map), before);
    }

    #[test]
    fn moving_a_story_writes_one_story_and_no_other() {
        let (mut map, activities, releases) = map();
        let from = CellRef::new(activities[0], releases[0]);
        let to = CellRef::new(activities[2], releases[1]);
        let moved = map.stories_in(from)[0].id();

        let untouched: Vec<(CellRef, String, Rank)> = census(&map)
            .into_iter()
            .filter(|(_, label, _)| label != map.story(moved).unwrap().label())
            .collect();

        let write = map.move_story(moved, to, Slot::Index(1)).unwrap();
        assert_eq!(write.from, from);
        assert_eq!(write.to, to);

        let after: Vec<(CellRef, String, Rank)> = census(&map)
            .into_iter()
            .filter(|(_, label, _)| label != map.story(moved).unwrap().label())
            .collect();
        assert_eq!(after, untouched);

        assert_eq!(map.locate(moved), Some((to, 1)));
        assert_eq!(map.stories_in(from).len(), 1);
        assert_eq!(map.stories_in(to).len(), 3);
        assert_eq!(map.story_count(), 18);
    }

    #[test]
    fn a_cell_stops_existing_when_its_last_story_leaves() {
        let mut map = StoryMap::new("m");
        let activity = map.add_activity("a");
        let release = map.add_release("r");
        let other = map.add_release("r2");
        let at = CellRef::new(activity, release);

        let story = map.add_story(at, "only").unwrap();
        assert_eq!(map.cells().len(), 1);
        map.move_story(story, CellRef::new(activity, other), Slot::Top).unwrap();
        assert_eq!(map.cells().len(), 1, "the source cell is gone, the destination exists");
        assert_eq!(map.cells()[0].at().release, other);
        map.remove_story(story).unwrap();
        assert!(map.cells().is_empty());
        assert!(map.stories_in(at).is_empty());
    }

    #[test]
    fn a_story_cannot_be_put_in_a_cell_that_does_not_exist() {
        let (mut map, activities, releases) = map();
        let snapshot = serde_json::to_string(&map).unwrap();
        let ghost_activity = ActivityId::from_raw(9999);
        let ghost_release = ReleaseId::from_raw(9999);
        let story = map.stories_in(CellRef::new(activities[0], releases[0]))[0].id();

        assert_eq!(
            map.add_story(CellRef::new(ghost_activity, releases[0]), "x"),
            Err(FlowError::NoSuchActivity(ghost_activity))
        );
        assert_eq!(
            map.add_story(CellRef::new(activities[0], ghost_release), "x"),
            Err(FlowError::NoSuchRelease(ghost_release))
        );
        assert_eq!(
            map.move_story(story, CellRef::new(activities[0], ghost_release), Slot::Top),
            Err(FlowError::NoSuchRelease(ghost_release))
        );
        assert_eq!(
            map.remove_activity(ghost_activity),
            Err(FlowError::NoSuchActivity(ghost_activity))
        );
        assert_eq!(serde_json::to_string(&map).unwrap(), snapshot, "a refusal changes nothing");
    }

    #[test]
    fn columns_and_rows_reorder_without_disturbing_their_cells() {
        let (mut map, activities, releases) = map();
        let before = census(&map);
        map.move_activity(activities[0], 2).unwrap();
        map.move_release(releases[2], 0).unwrap();
        assert_eq!(census(&map), before);
        assert_eq!(map.activity_index(activities[0]), Some(2));
        assert_eq!(map.release_index(releases[2]), Some(0));
        // Past the end clamps rather than failing.
        map.move_activity(activities[0], 99).unwrap();
        assert_eq!(map.activity_index(activities[0]), Some(2));
    }

    #[test]
    fn a_map_survives_a_save_and_load_unchanged() {
        let (mut map, activities, releases) = map();
        let story = map.stories_in(CellRef::new(activities[0], releases[0]))[1].id();
        map.move_story(story, CellRef::new(activities[2], releases[2]), Slot::Top).unwrap();
        map.set_story_height(story, Some(72.0)).unwrap();
        map.rename_activity(activities[0], "Discover").unwrap();

        let json = serde_json::to_string(&map).unwrap();
        let loaded: StoryMap = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded, map);
        assert_eq!(serde_json::to_string(&loaded).unwrap(), json);
    }
}
