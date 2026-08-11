//! Hit-testing a laid-out table: which cell, or which boundary to drag.
//!
//! # Tolerance is a world distance, and the caller has to scale it
//!
//! [`TableLayout::hit_test`] takes a tolerance in **world px**, because that is the
//! space the layout is in. The number the user experiences is a screen distance —
//! about 4px either side of a boundary is the usual grab band — so the caller passes
//! `4.0 / zoom`. Passing a constant instead would make boundaries impossible to grab
//! when zoomed out and would swallow whole cells when zoomed in, which is a bug
//! worth naming here because the type system cannot.
//!
//! # Boundaries win over cells, and columns win over rows
//!
//! A point near a boundary reports the boundary even though it is also inside a
//! cell: you cannot resize a column you cannot grab, whereas you can always click
//! the middle of a cell. At a corner, where a row boundary and a column boundary are
//! both in range, the column wins — horizontal resizing is the dominant gesture in
//! every table, and a 4×4px corner is not a target anyone aims at deliberately.
//!
//! # Which tracks a boundary resizes
//!
//! Boundary `i` has column `i - 1` before it and column `i` after it, either of
//! which may be absent at the table's edges. Which one a drag actually changes is
//! the app's decision, not this crate's: dragging the interior boundary of a natural
//! table normally resizes the column to its left and pushes everything right, while
//! dragging inside a table pinned to a width has to take from one and give to the
//! other. [`HitTarget::adjacent`] gives both indices; the policy stays where the
//! gesture is.

use serde::{Deserialize, Serialize};

use crate::geometry::{EPSILON, Point};
use crate::grid::CellRef;
use crate::layout::{Orientation, TableLayout};

/// What is under a point.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HitTarget {
    /// Inside a cell. Always the cell's **anchor**, so clicking anywhere in a merged
    /// block selects the block.
    Cell { anchor: CellRef },
    /// On the vertical line at column boundary `index`, `0..=columns`.
    ColumnBoundary { index: usize },
    /// On the horizontal line at row boundary `index`, `0..=rows`.
    RowBoundary { index: usize },
    /// Not on the table at all.
    Outside,
}

impl HitTarget {
    pub fn cell(self) -> Option<CellRef> {
        match self {
            Self::Cell { anchor } => Some(anchor),
            _ => None,
        }
    }

    /// The boundary, if this is one.
    pub fn boundary(self) -> Option<(Orientation, usize)> {
        match self {
            Self::ColumnBoundary { index } => Some((Orientation::Vertical, index)),
            Self::RowBoundary { index } => Some((Orientation::Horizontal, index)),
            _ => None,
        }
    }

    /// The two tracks a boundary sits between — `(before, after)`, either of which
    /// is `None` at the table's edge. `count` is the number of columns for a column
    /// boundary, rows for a row boundary.
    pub fn adjacent(self, count: usize) -> Option<(Option<usize>, Option<usize>)> {
        let (_, index) = self.boundary()?;
        Some((index.checked_sub(1), (index < count).then_some(index)))
    }

    pub fn is_boundary(self) -> bool {
        matches!(self, Self::ColumnBoundary { .. } | Self::RowBoundary { .. })
    }
}

impl TableLayout {
    /// The column containing `x`, or `None` if `x` is outside the table.
    ///
    /// Half-open, matching [`Rect::contains`](crate::Rect::contains): a coordinate
    /// exactly on a boundary belongs to the column that starts there. A zero-width
    /// column contains nothing and is therefore never returned.
    pub fn column_at(&self, x: f64) -> Option<usize> {
        track_at(&self.column_offsets, self.columns(), x)
    }

    /// The row containing `y`, or `None` if `y` is outside the table.
    pub fn row_at(&self, y: f64) -> Option<usize> {
        track_at(&self.row_offsets, self.rows(), y)
    }

    /// The nearest column boundary and its distance. `None` only for a table with no
    /// columns, which has no boundaries at all — not even one.
    pub fn nearest_column_boundary(&self, x: f64) -> Option<(usize, f64)> {
        if self.columns() == 0 {
            return None;
        }
        nearest(&self.column_offsets, x)
    }

    pub fn nearest_row_boundary(&self, y: f64) -> Option<(usize, f64)> {
        if self.rows() == 0 {
            return None;
        }
        nearest(&self.row_offsets, y)
    }

    /// What is under `point`, with boundaries grabbed from `tolerance` world px away.
    /// See the module docs for the tolerance units and the precedence rules.
    pub fn hit_test(&self, point: Point, tolerance: f64) -> HitTarget {
        let tolerance = tolerance.max(0.0);
        if self.rows() == 0 || self.columns() == 0 {
            return HitTarget::Outside;
        }
        let bounds = self.bounds();
        let outside = point.x < bounds.left() - tolerance
            || point.x > bounds.right() + tolerance
            || point.y < bounds.top() - tolerance
            || point.y > bounds.bottom() + tolerance;
        if outside {
            return HitTarget::Outside;
        }

        if let Some((index, distance)) = self.nearest_column_boundary(point.x)
            && distance <= tolerance
        {
            return HitTarget::ColumnBoundary { index };
        }
        if let Some((index, distance)) = self.nearest_row_boundary(point.y)
            && distance <= tolerance
        {
            return HitTarget::RowBoundary { index };
        }

        // Inside the tolerance band around the table but past its edge: clamp in,
        // rather than reporting Outside for a point the boundary test already
        // decided was not close enough to a boundary to matter.
        let x = point.x.clamp(bounds.left(), bounds.right());
        let y = point.y.clamp(bounds.top(), bounds.bottom());
        match (self.column_at(x), self.row_at(y)) {
            (Some(col), Some(row)) => match self.cell_at(CellRef::new(row, col)) {
                Some(cell) => HitTarget::Cell { anchor: cell.anchor },
                None => HitTarget::Outside,
            },
            _ => HitTarget::Outside,
        }
    }
}

fn track_at(offsets: &[f64], count: usize, v: f64) -> Option<usize> {
    if count == 0 || v < offsets[0] || v >= offsets[count] {
        return None;
    }
    // First boundary strictly past `v`; the track before it is the one containing
    // `v`. A run of equal offsets — collapsed columns — is skipped in one step,
    // which is why a zero-width track is never reported as containing anything.
    let past = offsets.partition_point(|&o| o <= v);
    Some(past.saturating_sub(1).min(count - 1))
}

/// The nearest boundary and its distance.
///
/// Ties between *coincident* boundaries go to the **last** of them. That happens
/// exactly when the tracks between them have collapsed to zero width, and it is the
/// difference between a collapsed column being recoverable and not: boundary `i`
/// resizes column `i - 1`, so grabbing the last boundary of the run is what puts the
/// drag on the collapsed column rather than on its intact neighbour.
fn nearest(offsets: &[f64], v: f64) -> Option<(usize, f64)> {
    if offsets.is_empty() {
        return None;
    }
    let at_or_after = offsets.partition_point(|&o| o < v);
    let mut best = at_or_after.min(offsets.len() - 1);
    let mut distance = (offsets[best] - v).abs();
    if at_or_after > 0 {
        let before = (offsets[at_or_after - 1] - v).abs();
        if before < distance {
            best = at_or_after - 1;
            distance = before;
        }
    }
    // Walk to the end of any run of boundaries sharing this coordinate. Every one of
    // them is exactly as near, so this changes which index is reported and nothing
    // else.
    while best + 1 < offsets.len() && (offsets[best + 1] - offsets[best]).abs() <= EPSILON {
        best += 1;
    }
    Some((best, distance))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grid::CellRange;
    use crate::measure::{MeasureCache, MonospaceMeasure};
    use crate::sizing::{Fit, Sizing};
    use crate::table::Table;

    /// A table of three 100×40 cells per row, at the origin.
    fn grid_table(rows: usize, columns: usize) -> TableLayout {
        let mut table = Table::new(rows, columns);
        for i in 0..columns {
            table.set_column_sizing(i, Sizing::Fixed(100.0)).unwrap();
        }
        for i in 0..rows {
            table.set_row_sizing(i, Sizing::Fixed(40.0)).unwrap();
        }
        table.layout(
            &mut MonospaceMeasure::default(),
            &mut MeasureCache::new(),
            Fit::Natural,
            Point::ORIGIN,
        )
    }

    #[test]
    fn a_point_in_the_middle_of_a_cell_reports_that_cell() {
        let layout = grid_table(3, 3);
        assert_eq!(
            layout.hit_test(Point::new(150.0, 60.0), 4.0),
            HitTarget::Cell { anchor: CellRef::new(1, 1) }
        );
    }

    #[test]
    fn a_point_near_a_boundary_reports_the_boundary_not_the_cell() {
        let layout = grid_table(3, 3);
        assert_eq!(
            layout.hit_test(Point::new(202.0, 60.0), 4.0),
            HitTarget::ColumnBoundary { index: 2 }
        );
        assert_eq!(
            layout.hit_test(Point::new(150.0, 79.0), 4.0),
            HitTarget::RowBoundary { index: 2 }
        );
    }

    /// The table's own edges have to be grabbable, or the last column can never be
    /// resized.
    #[test]
    fn the_outer_edges_are_boundaries_too() {
        let layout = grid_table(2, 2);
        assert_eq!(
            layout.hit_test(Point::new(-2.0, 20.0), 4.0),
            HitTarget::ColumnBoundary { index: 0 }
        );
        assert_eq!(
            layout.hit_test(Point::new(201.0, 20.0), 4.0),
            HitTarget::ColumnBoundary { index: 2 }
        );
        assert_eq!(layout.hit_test(Point::new(300.0, 20.0), 4.0), HitTarget::Outside);
    }

    #[test]
    fn at_a_corner_the_column_boundary_wins() {
        let layout = grid_table(3, 3);
        assert_eq!(
            layout.hit_test(Point::new(100.0, 40.0), 4.0),
            HitTarget::ColumnBoundary { index: 1 }
        );
    }

    #[test]
    fn clicking_anywhere_in_a_merged_block_reports_its_anchor() {
        let mut table = Table::new(3, 3);
        for i in 0..3 {
            table.set_column_sizing(i, Sizing::Fixed(100.0)).unwrap();
            table.set_row_sizing(i, Sizing::Fixed(40.0)).unwrap();
        }
        table.merge(CellRange::new(0, 0, 2, 2), Default::default()).unwrap();
        let layout = table.layout(
            &mut MonospaceMeasure::default(),
            &mut MeasureCache::new(),
            Fit::Natural,
            Point::ORIGIN,
        );
        assert_eq!(
            layout.hit_test(Point::new(150.0, 60.0), 4.0),
            HitTarget::Cell { anchor: CellRef::new(0, 0) },
            "the bottom-right of the merge still reports (0,0)"
        );
    }

    #[test]
    fn a_table_with_no_rows_has_nothing_to_hit() {
        let layout = grid_table(0, 3);
        assert_eq!(layout.hit_test(Point::new(50.0, 0.0), 4.0), HitTarget::Outside);
        assert_eq!(layout.column_at(50.0), Some(0), "the columns are still resolved");
        assert_eq!(layout.row_at(0.0), None);
    }

    #[test]
    fn a_collapsed_column_hands_its_boundary_to_the_drag_that_can_restore_it() {
        let mut table = Table::new(1, 3);
        table.set_column_sizing(0, Sizing::Fixed(100.0)).unwrap();
        table.set_column_sizing(1, Sizing::Fixed(0.0)).unwrap();
        table.set_column_sizing(2, Sizing::Fixed(100.0)).unwrap();
        let layout = table.layout(
            &mut MonospaceMeasure::default(),
            &mut MeasureCache::new(),
            Fit::Natural,
            Point::ORIGIN,
        );
        assert_eq!(layout.column_at(100.0), Some(2), "the zero-width column contains nothing");
        assert_eq!(layout.nearest_column_boundary(100.0), Some((2, 0.0)));
        assert_eq!(
            HitTarget::ColumnBoundary { index: 2 }.adjacent(3),
            Some((Some(1), Some(2))),
            "boundary 2 is between the collapsed column and the one after it"
        );
    }

    #[test]
    fn boundary_helpers_report_the_tracks_on_either_side() {
        assert_eq!(HitTarget::ColumnBoundary { index: 0 }.adjacent(3), Some((None, Some(0))));
        assert_eq!(HitTarget::ColumnBoundary { index: 3 }.adjacent(3), Some((Some(2), None)));
        assert_eq!(HitTarget::Outside.adjacent(3), None);
        assert_eq!(HitTarget::Cell { anchor: CellRef::new(0, 0) }.cell(), Some(CellRef::new(0, 0)));
    }
}
