//! Vellum's table widget: the grid, the sizing and the geometry. No GPU, no
//! document, no window.
//!
//! A table is the one Miro widget that is really a *layout container* — its cells
//! are not free-floating items but a grid whose parts move when any of them changes.
//! `docs/features/README.md` §2 lists it as "rows/columns, merge, header styling,
//! resize", and every one of those four words is a constraint on the other three:
//! a merge has to survive a row being deleted, a header has to restyle without
//! rewriting text, and a resize has to not reflow the table it is part of.
//!
//! ```
//! use vellum_table::{
//!     CellRange, CellRef, Fit, HitTarget, MeasureCache, MergeContent, MonospaceMeasure,
//!     Point, Table,
//! };
//!
//! let mut table = Table::new(3, 3);
//! table.set_header_rows(1);
//! for (col, name) in ["Part", "Torque", "Notes"].iter().enumerate() {
//!     table.set_content(CellRef::new(0, col), *name).unwrap();
//! }
//! table.set_content(CellRef::new(1, 0), "M10 bolt").unwrap();
//!
//! // A note spanning the two right-hand columns of the last row.
//! table.merge(CellRange::new(2, 1, 1, 2), MergeContent::Concatenate).unwrap();
//! assert_eq!(table.cell_count(), 8); // nine positions, one of them merged away
//!
//! // Absolute rectangles, at an origin, in the width available.
//! let layout = table.layout(
//!     &mut MonospaceMeasure::default(),
//!     &mut MeasureCache::new(),
//!     Fit::Width(600.0),
//!     Point::ORIGIN,
//! );
//! assert!((layout.size.width - 600.0).abs() < 1e-9);
//!
//! let heading = layout.cell_at(CellRef::new(0, 0)).unwrap();
//! assert_eq!(heading.rect.left(), 0.0);
//! assert!(heading.style.text.bold, "header rows are bold without their text being rewritten");
//!
//! // Point in, cell or draggable boundary out.
//! assert_eq!(
//!     layout.hit_test(Point::new(15.0, 15.0), 4.0),
//!     HitTarget::Cell { anchor: CellRef::new(0, 0) }
//! );
//! assert_eq!(
//!     layout.hit_test(Point::new(layout.column_offsets[1], 15.0), 4.0),
//!     HitTarget::ColumnBoundary { index: 1 }
//! );
//! ```
//!
//! # The one decision everything else follows from
//!
//! **Auto-fit means max-content, and does not depend on the table's width.**
//!
//! The alternative — CSS's real table algorithm, where a column's width depends on
//! the space available — makes column width, row height and table height mutually
//! recursive, so every column drag re-solves the whole table. That is the quadratic
//! reflow `docs/01-architecture.md` §3 exists to prevent, and it is why dragging a
//! column in a large Miro board is visibly slow.
//!
//! Because auto-fit is content-only, a cell's intrinsic width is a pure function of
//! its text and can be cached on the cell's identity. Dragging column *j* then
//! re-measures the cells in column *j* — they must rewrap — and reads every other
//! column from [`MeasureCache`]. `O(rows)`, not `O(rows × columns)`, and
//! [`MeasureStats`] makes that a test rather than a claim.
//!
//! What it costs is a column that could have been narrower under pressure. That is
//! what a track's `min`/`max` and [`Fit::Width`] are for.
//!
//! # Layout
//!
//! - [`mod@table`] — [`Table`]: the grid plus per-row and per-column specs, and the
//!   edits that keep them consistent.
//! - [`grid`] — merges: the dense slot representation, the four invariants, and the
//!   insert/delete cases that would otherwise orphan a cell.
//! - [`sizing`] — fixed, auto and proportional tracks, and how leftover space and
//!   span deficits are shared out.
//! - [`measure`] — the [`Measure`] seam onto `vellum-text`, a font-free stand-in for
//!   tests, and the cache the performance argument rests on.
//! - [`mod@layout`] — the seven passes, and the border deduplication.
//! - [`hit`] — point → cell, and point → the boundary you would drag.
//! - [`mod@style`] — the six-level cascade. Contains no colour literals, on purpose.
//! - [`span`] — styled text, mirroring `vellum-text` without depending on it.
//! - [`geometry`] — `f64` world-space points, rects and insets.
//!
//! # What is not here
//!
//! - **Rendering.** [`TableLayout`] is data; `vellum-render` draws it.
//! - **Editing.** Caret movement, tab-to-next-cell and selection are the app's, over
//!   `vellum-text`'s editor. What this crate provides is what that needs:
//!   [`CellLayout::content_rect`] to put a caret in, and [`TableLayout::hit_test`]
//!   to know which cell was clicked.
//! - **Import.** Miro's own tables are not in the reference board's clipboard
//!   payload — `docs/02-miro-formats.md` lists `table` among the widget types still
//!   unobserved — so there is no mapping to write yet, and inventing one from the
//!   encrypted `tables.json` would be guesswork. The model is shaped to receive one:
//!   spans, per-cell style overrides and merges are all already here.

pub mod error;
pub mod geometry;
pub mod grid;
pub mod hit;
pub mod layout;
pub mod measure;
pub mod sizing;
pub mod span;
pub mod style;
pub mod table;

pub use error::{Result, TableError};
pub use geometry::{EPSILON, Insets, Point, Rect, Size};
pub use grid::{Cell, CellId, CellRange, CellRef, Grid, GridSpan, MergeContent, Slot};
pub use hit::HitTarget;
pub use layout::{BorderSegment, CellLayout, Orientation, TableLayout};
pub use measure::{Intrinsic, Measure, MeasureCache, MeasureStats, MonospaceMeasure};
pub use sizing::{
    Fit, MAX_AUTO_COLUMN_WIDTH, MIN_COLUMN_WIDTH, MIN_ROW_HEIGHT, MIN_TRACK_SIZE, Sizing,
    TrackIntrinsic,
};
pub use span::{Rgb, SpanStyle, StyledText, TextSpan};
pub use style::{
    BorderDash, BorderSide, Borders, CellKind, CellStyle, DEFAULT_FONT_SIZE, DEFAULT_LINE_HEIGHT,
    ResolvedCellStyle, Rgba, TableStyle, TextAlign, TextStyle, VerticalAlign,
};
pub use table::{ColumnSpec, RowSpec, Table};
