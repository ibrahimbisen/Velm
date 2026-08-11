//! Vellum's board containers: kanban, user story maps, and timelines. Pure logic —
//! no GPU, no document, no window.
//!
//! `docs/features/README.md` §2 lists these three among the structured widgets, and
//! they are the ones that are really **layout containers**: their children are not
//! free-floating items but a column, a cell or a lane whose position is decided by
//! the container. Move a card and its neighbours move; delete a column and everything
//! under it has to go somewhere. That is what this crate is: the ownership rules, the
//! ordering, and the arithmetic that turns them into rectangles.
//!
//! They also matter for import. **No Miro API exposes any of the three** — a kanban
//! comes out of the clipboard as loose stickies with no columns, and the structure
//! has to be reconstructed. A model that can express what was on the original board
//! is the precondition for ever getting that right.
//!
//! ```
//! use vellum_flow::{DateRange, Kanban, Point, Rect, Slot, StoryMap, Timeline};
//!
//! // Kanban: columns of cards, moved by a single write.
//! let mut board = Kanban::new("Sprint 14");
//! let todo = board.add_column("To do");
//! let doing = board.add_column("Doing");
//! let card = board.add_card(todo, "SDF corners").unwrap();
//! board.move_card(card, doing, Slot::Top).unwrap();
//!
//! // Any container turns an area into absolute rectangles for its chrome and every
//! // child, and turns a point back into the child under it.
//! let layout = board.layout(Rect::new(0.0, 0.0, 900.0, 600.0));
//! let placed = layout.card(card).unwrap().rect;
//! assert!(!placed.is_empty());
//!
//! // And previews a drop before it is committed.
//! let over = Point::new(placed.centre().x, placed.bottom() + 200.0);
//! assert_eq!(layout.drop_target(over, Some(card)).unwrap().column, doing);
//!
//! // Timelines pack bars into as few lanes as will hold them without overlap.
//! let mut plan = Timeline::new("Q3");
//! let day = |text: &str| text.parse().unwrap();
//! plan.add_bar("Shaping", DateRange::new(day("2026-08-01"), day("2026-08-20")));
//! plan.add_bar("Atlas", DateRange::new(day("2026-08-10"), day("2026-09-05")));
//! plan.add_bar("Ship", DateRange::new(day("2026-09-06"), day("2026-09-12")));
//! assert_eq!(plan.lanes().lane_count, 2);
//!
//! let map = StoryMap::new("Onboarding");
//! assert!(map.layout(Rect::new(0.0, 0.0, 900.0, 600.0)).cells.is_empty());
//! ```
//!
//! # What every container here does
//!
//! The three widgets are different shapes, but the same four things are asked of all
//! of them, and they answer in the same vocabulary:
//!
//! 1. **Hold children in an order**, and let that order be edited without disturbing
//!    anything else. Every child carries a [`Rank`] — a fractional key, the same
//!    technique `docs/01-architecture.md` §4 uses for z-order — so moving one child
//!    writes one child. See [`rank`] for the arithmetic and for why byte order is the
//!    ordering.
//! 2. **Lay out into a given area**, producing absolute rectangles for the container
//!    chrome and for every child. `layout(area)`, on each of [`Kanban`],
//!    [`StoryMap`] and [`Timeline`].
//! 3. **Hit-test a point** back to the cell, column or lane under it. `hit_test`, on
//!    each layout.
//! 4. **Preview a drop** before it is committed. `drop_target`, on each layout,
//!    returning both where the child would land and the rectangle to draw. The tests
//!    assert that the two agree exactly: what the preview draws is where the child
//!    ends up.
//!
//! # What it deliberately does not do
//!
//! - **No measurement.** A card's height is whatever the caller measured, or a
//!   default from [`metrics`]. Depending on `vellum-text` so a container could size
//!   its own labels would make every test here load fonts.
//! - **No colour.** Layout produces geometry and state — a WIP breach, a violated
//!   dependency — and `vellum-ui` resolves those to tokens.
//!   `docs/05-design-language.md` §1 forbids a hex literal in a widget, and this
//!   crate has none.
//! - **No clock.** [`Timeline`] never asks what today is; see [`date`].
//! - **No scrolling or clipping.** When the content needs more room than the area
//!   given, the layout says so (`content`, `overflows()`) and lays the overflow out
//!   past the edge. Squeezing twelve kanban columns into 600px produces a board
//!   nobody can read and hides the fact that it did.
//!
//! # Layout
//!
//! - [`kanban`] — columns, WIP limits, cards.
//! - [`story_map`] — activities × releases, stories in the cells.
//! - [`timeline`] — bars, lane packing, dependencies, the date axis.
//! - [`rank`] — fractional ordering keys, shared by the first two.
//! - [`slot`] — where a child goes when it is inserted or dropped.
//! - [`date`] — calendar dates and inclusive day ranges.
//! - [`geometry`] — points, sizes, rectangles.
//! - [`metrics`] — every number the layouts depend on, on a 4px grid.
//! - [`error`] — the ways an edit is refused.

pub mod date;
pub mod error;
pub mod geometry;
pub mod id;
pub mod kanban;
pub mod metrics;
pub mod rank;
pub mod slot;
pub mod story_map;
pub mod timeline;

pub use date::{Date, DateRange, Weekday};
pub use error::FlowError;
pub use geometry::{Insets, Point, Rect, Size};
pub use id::{ActivityId, BarId, CardId, ColumnId, ReleaseId, StoryId};
pub use kanban::{
    CardLayout, ColumnLayout, Kanban, KanbanDrop, KanbanLayout, KanbanTarget, Wip, WipBreach,
    WipStatus,
};
pub use metrics::{GRID, KanbanMetrics, RADIUS, StoryMapMetrics, TimelineMetrics};
pub use rank::Rank;
pub use slot::Slot;
pub use story_map::{CellRef, StoryMap, StoryMapDrop, StoryMapLayout, StoryMapTarget};
pub use timeline::{
    AxisSpan, BarDrag, BarEdge, LanePolicy, LinkKind, TickScale, TimeAxis, Timeline, TimelineDrop,
    TimelineLayout, TimelineTarget,
};
