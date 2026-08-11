//! The widget: a grid, a sizing and a style per track, two header counts, and the
//! operations that keep all of them consistent with each other.
//!
//! [`Table`] owns three parallel things — the [`Grid`] of cells, a [`RowSpec`] per
//! row and a [`ColumnSpec`] per column — and is the only type allowed to change the
//! shape of any of them, because a structural edit has to change all three at once.
//! [`Table::validate`] checks they still agree, so a divergence is a failing test
//! rather than an index panic during a frame.
//!
//! # A table has no position
//!
//! There is no `x` or `y` here, and no width either. Where the widget sits is the
//! document's business; how much room it has is the caller's, passed to
//! [`Table::layout`] as a [`Fit`]. This is the same separation `vellum-shapes` makes
//! by defining every shape in a unit box, and it has the same payoff: moving a table
//! invalidates nothing, and the same table can be laid out twice — once for the
//! board and once for an export at a different width — without being mutated in
//! between.

use serde::{Deserialize, Serialize};

use crate::error::{Result, TableError};
use crate::geometry::Point;
use crate::grid::{Cell, CellId, CellRange, CellRef, Grid, MergeContent};
use crate::layout::TableLayout;
use crate::measure::{Measure, MeasureCache};
use crate::sizing::{Fit, MIN_TRACK_SIZE, Sizing};
use crate::span::StyledText;
use crate::style::{CellKind, CellStyle, ResolvedCellStyle, TableStyle};

/// A row's height rule and the style it contributes to its cells.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RowSpec {
    pub sizing: Sizing,
    /// Applied to every cell in the row, above the column's style and below the
    /// cell's own. Striping a table is setting one of these per alternate row.
    pub style: CellStyle,
}

impl Default for RowSpec {
    fn default() -> Self {
        Self { sizing: Sizing::auto_row(), style: CellStyle::default() }
    }
}

/// A column's width rule and the style it contributes to its cells.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ColumnSpec {
    pub sizing: Sizing,
    pub style: CellStyle,
}

impl Default for ColumnSpec {
    fn default() -> Self {
        Self { sizing: Sizing::auto_column(), style: CellStyle::default() }
    }
}

/// A table widget.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Table {
    grid: Grid,
    rows: Vec<RowSpec>,
    columns: Vec<ColumnSpec>,
    header_rows: usize,
    header_columns: usize,
    style: TableStyle,
}

impl Table {
    /// A table of empty cells, every column auto-fitting and every row auto-fitting.
    ///
    /// `0 × n` and `n × 0` are legal. They are also reachable — deleting the last row
    /// of a table produces one — so everything downstream copes with them rather than
    /// assuming a table has area.
    pub fn new(rows: usize, columns: usize) -> Self {
        Self {
            grid: Grid::new(rows, columns),
            rows: vec![RowSpec::default(); rows],
            columns: vec![ColumnSpec::default(); columns],
            header_rows: 0,
            header_columns: 0,
            style: TableStyle::default(),
        }
    }

    pub fn row_count(&self) -> usize {
        self.rows.len()
    }

    pub fn column_count(&self) -> usize {
        self.columns.len()
    }

    /// Number of real cells: `rows × columns` less the positions merges have
    /// swallowed.
    pub fn cell_count(&self) -> usize {
        self.grid.cell_count()
    }

    /// True when the table encloses no positions — no rows, or no columns.
    pub fn is_empty(&self) -> bool {
        self.grid.is_empty()
    }

    pub fn rows(&self) -> &[RowSpec] {
        &self.rows
    }

    pub fn columns(&self) -> &[ColumnSpec] {
        &self.columns
    }

    pub fn grid(&self) -> &Grid {
        &self.grid
    }

    pub fn style(&self) -> &TableStyle {
        &self.style
    }

    pub fn style_mut(&mut self) -> &mut TableStyle {
        &mut self.style
    }

    pub fn row_mut(&mut self, index: usize) -> Result<&mut RowSpec> {
        let count = self.rows.len();
        self.rows.get_mut(index).ok_or(TableError::RowOutOfBounds { index, count })
    }

    pub fn column_mut(&mut self, index: usize) -> Result<&mut ColumnSpec> {
        let count = self.columns.len();
        self.columns.get_mut(index).ok_or(TableError::ColumnOutOfBounds { index, count })
    }

    pub fn set_row_sizing(&mut self, index: usize, sizing: Sizing) -> Result<()> {
        self.row_mut(index)?.sizing = sizing;
        Ok(())
    }

    pub fn set_column_sizing(&mut self, index: usize, sizing: Sizing) -> Result<()> {
        self.column_mut(index)?.sizing = sizing;
        Ok(())
    }

    /// Pins a column to a width, as dragging its boundary does.
    ///
    /// **A proportional column becomes fixed.** A drag whose result the next reflow
    /// immediately discards is not a resize, and leaving the column proportional
    /// would do exactly that the moment the table's own width changed.
    ///
    /// Clamped to [`MIN_TRACK_SIZE`] rather than to the auto minimum: the model
    /// allows a 4px spacer column, but a *drag* must not be able to produce a column
    /// with no boundary left to grab.
    pub fn resize_column_to(&mut self, index: usize, width: f64) -> Result<()> {
        self.column_mut(index)?.sizing = Sizing::Fixed(width.max(MIN_TRACK_SIZE));
        Ok(())
    }

    /// Pins a row to a height. See [`resize_column_to`](Self::resize_column_to).
    pub fn resize_row_to(&mut self, index: usize, height: f64) -> Result<()> {
        self.row_mut(index)?.sizing = Sizing::Fixed(height.max(MIN_TRACK_SIZE));
        Ok(())
    }

    /// How many leading rows are headers.
    pub fn header_rows(&self) -> usize {
        self.header_rows
    }

    pub fn header_columns(&self) -> usize {
        self.header_columns
    }

    /// Clamped to the table's size, so a table that shrinks below its own header
    /// count does not end up claiming rows it no longer has.
    pub fn set_header_rows(&mut self, count: usize) {
        self.header_rows = count.min(self.rows.len());
    }

    pub fn set_header_columns(&mut self, count: usize) {
        self.header_columns = count.min(self.columns.len());
    }

    /// Where the cell occupying a position is anchored — itself, unless it is inside
    /// a merge. Every other method here takes the anchor.
    pub fn anchor_at(&self, at: CellRef) -> Option<CellRef> {
        self.grid.anchor_at(at)
    }

    /// The cell anchored exactly here.
    pub fn cell(&self, at: CellRef) -> Option<&Cell> {
        self.grid.cell(at)
    }

    /// The cell occupying this position, resolving through a merge.
    pub fn cell_at(&self, at: CellRef) -> Option<(CellRef, &Cell)> {
        self.grid.cell_at(at)
    }

    pub fn cell_mut(&mut self, at: CellRef) -> Result<&mut Cell> {
        self.grid.cell_mut(at)
    }

    pub fn set_content(&mut self, at: CellRef, content: impl Into<StyledText>) -> Result<()> {
        self.grid.cell_mut(at)?.set_content(content);
        Ok(())
    }

    /// Every cell's id, for [`MeasureCache::prune`](crate::MeasureCache::prune).
    pub fn cell_ids(&self) -> Vec<CellId> {
        self.grid.anchors().map(|(_, cell)| cell.id()).collect()
    }

    /// Where a cell sits relative to the headers. Determined by the **anchor**: a
    /// merged cell that starts in the header row is a header cell for its whole
    /// height, which is what a two-row title block wants.
    pub fn cell_kind(&self, at: CellRef) -> CellKind {
        CellKind::of(at.row < self.header_rows, at.col < self.header_columns)
    }

    /// The six-level cascade, resolved. See [`style`](crate::style) for the order
    /// and why it is that order.
    pub fn resolved_style(&self, at: CellRef) -> ResolvedCellStyle {
        let mut resolved = self.style.cell.clone();
        if at.col < self.header_columns && !self.style.header_column.is_empty() {
            self.style.header_column.apply_to(&mut resolved);
        }
        if at.row < self.header_rows && !self.style.header_row.is_empty() {
            self.style.header_row.apply_to(&mut resolved);
        }
        if let Some(column) = self.columns.get(at.col)
            && !column.style.is_empty()
        {
            column.style.apply_to(&mut resolved);
        }
        if let Some(row) = self.rows.get(at.row)
            && !row.style.is_empty()
        {
            row.style.apply_to(&mut resolved);
        }
        if let Some(cell) = self.grid.cell(at)
            && !cell.style().is_empty()
        {
            cell.style().apply_to(&mut resolved);
        }
        resolved
    }

    /// Merges a range into one cell, returning the range actually merged — the
    /// requested one grown over any merge it clipped. See [`Grid::merge`].
    pub fn merge(&mut self, range: CellRange, policy: MergeContent) -> Result<CellRange> {
        self.grid.merge(range, policy)
    }

    pub fn unmerge(&mut self, at: CellRef) -> Result<()> {
        self.grid.unmerge(at)
    }

    /// Inserts an empty row at `index`, pushing the rest down.
    ///
    /// The new row is a **body** row even when inserted at the header boundary: a
    /// user adding a row directly below the header is adding data. Inserting
    /// strictly *inside* the header block extends it, because there is no other
    /// reading of that gesture.
    pub fn insert_row(&mut self, index: usize) -> Result<()> {
        self.grid.insert_row(index)?;
        self.rows.insert(index, RowSpec::default());
        if index < self.header_rows {
            self.header_rows += 1;
        }
        Ok(())
    }

    /// Removes a row, its spec, and its share of the header count.
    ///
    /// Merges that reach past the row survive it — see [`Grid::remove_row`] for the
    /// re-anchoring that makes that true.
    pub fn remove_row(&mut self, index: usize) -> Result<()> {
        self.grid.remove_row(index)?;
        self.rows.remove(index);
        if index < self.header_rows {
            self.header_rows -= 1;
        }
        Ok(())
    }

    pub fn insert_column(&mut self, index: usize) -> Result<()> {
        self.grid.insert_column(index)?;
        self.columns.insert(index, ColumnSpec::default());
        if index < self.header_columns {
            self.header_columns += 1;
        }
        Ok(())
    }

    pub fn remove_column(&mut self, index: usize) -> Result<()> {
        self.grid.remove_column(index)?;
        self.columns.remove(index);
        if index < self.header_columns {
            self.header_columns -= 1;
        }
        Ok(())
    }

    /// Lays the table out at `origin` in the room `fit` allows, producing absolute
    /// rectangles.
    ///
    /// `cache` may — and should — be reused across calls: it is what makes a resize
    /// cost `O(rows)` measurements rather than `O(rows × columns)`. It must belong to
    /// this `measure`; see [`MeasureCache`].
    pub fn layout<M: Measure + ?Sized>(
        &self,
        measure: &mut M,
        cache: &mut MeasureCache,
        fit: Fit,
        origin: Point,
    ) -> TableLayout {
        crate::layout::build(self, measure, cache, fit, origin)
    }

    /// Checks the grid's invariants *and* that the specs still match its shape.
    /// Cheap enough to call from a debug assertion after an edit, and every
    /// structural test in this crate ends here.
    pub fn validate(&self) -> Result<()> {
        self.grid.validate()?;
        if self.rows.len() != self.grid.rows() {
            return Err(TableError::invariant(
                CellRef::new(0, 0),
                "row specs do not match the grid",
            ));
        }
        if self.columns.len() != self.grid.columns() {
            return Err(TableError::invariant(
                CellRef::new(0, 0),
                "column specs do not match the grid",
            ));
        }
        if self.header_rows > self.rows.len() {
            return Err(TableError::invariant(CellRef::new(0, 0), "more header rows than rows"));
        }
        if self.header_columns > self.columns.len() {
            return Err(TableError::invariant(
                CellRef::new(0, 0),
                "more header columns than columns",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::style::{Rgba, TextAlign, VerticalAlign};

    #[test]
    fn a_new_table_is_all_auto_and_has_no_headers() {
        let table = Table::new(3, 4);
        assert_eq!(table.row_count(), 3);
        assert_eq!(table.column_count(), 4);
        assert_eq!(table.cell_count(), 12);
        assert_eq!(table.header_rows(), 0);
        assert!(table.columns().iter().all(|c| c.sizing == Sizing::auto_column()));
        assert!(table.rows().iter().all(|r| r.sizing == Sizing::auto_row()));
        table.validate().unwrap();
    }

    #[test]
    fn structural_edits_keep_the_specs_in_step_with_the_grid() {
        let mut table = Table::new(2, 2);
        table.set_column_sizing(0, Sizing::Fixed(120.0)).unwrap();
        table.insert_column(0).unwrap();
        assert_eq!(table.column_count(), 3);
        assert_eq!(table.columns()[0].sizing, Sizing::auto_column(), "the new column is fresh");
        assert_eq!(table.columns()[1].sizing, Sizing::Fixed(120.0), "the old one moved right");
        table.remove_column(1).unwrap();
        assert_eq!(table.columns()[0].sizing, Sizing::auto_column());
        table.validate().unwrap();
    }

    #[test]
    fn a_row_inserted_inside_the_header_block_joins_it_and_one_below_does_not() {
        let mut table = Table::new(4, 2);
        table.set_header_rows(2);
        table.insert_row(1).unwrap();
        assert_eq!(table.header_rows(), 3, "inserted between two header rows");

        let mut table = Table::new(4, 2);
        table.set_header_rows(2);
        table.insert_row(2).unwrap();
        assert_eq!(table.header_rows(), 2, "inserted at the boundary: a data row");
        table.validate().unwrap();
    }

    #[test]
    fn deleting_a_header_row_shrinks_the_header_block() {
        let mut table = Table::new(3, 2);
        table.set_header_rows(2);
        table.remove_row(0).unwrap();
        assert_eq!(table.header_rows(), 1);
        table.remove_row(1).unwrap();
        assert_eq!(table.header_rows(), 1, "deleting a body row leaves the header alone");
        table.validate().unwrap();
    }

    #[test]
    fn the_header_count_is_clamped_to_a_table_that_has_shrunk() {
        let mut table = Table::new(2, 2);
        table.set_header_rows(9);
        assert_eq!(table.header_rows(), 2);
        table.validate().unwrap();
    }

    /// The cascade order is a decision, so it is asserted rather than assumed.
    #[test]
    fn the_cascade_runs_table_then_headers_then_column_then_row_then_cell() {
        let mut table = Table::new(2, 2);
        table.set_header_rows(1);
        table.set_header_columns(1);

        table.style_mut().cell.text.align = TextAlign::Left;
        table.column_mut(0).unwrap().style.align = Some(TextAlign::Center);
        table.row_mut(0).unwrap().style.align = Some(TextAlign::Right);

        // Row beats column.
        assert_eq!(table.resolved_style(CellRef::new(0, 0)).text.align, TextAlign::Right);
        // Column applies where the row says nothing.
        assert_eq!(table.resolved_style(CellRef::new(1, 0)).text.align, TextAlign::Center);
        // Neither applies elsewhere.
        assert_eq!(table.resolved_style(CellRef::new(1, 1)).text.align, TextAlign::Left);

        // The cell beats everything.
        table.cell_mut(CellRef::new(0, 0)).unwrap().style_mut().align = Some(TextAlign::Left);
        assert_eq!(table.resolved_style(CellRef::new(0, 0)).text.align, TextAlign::Left);
    }

    #[test]
    fn a_header_row_is_bold_by_default_and_a_header_column_too() {
        let mut table = Table::new(2, 2);
        table.set_header_rows(1);
        table.set_header_columns(1);
        assert!(table.resolved_style(CellRef::new(0, 1)).text.bold);
        assert!(table.resolved_style(CellRef::new(1, 0)).text.bold);
        assert!(!table.resolved_style(CellRef::new(1, 1)).text.bold);
        assert_eq!(table.cell_kind(CellRef::new(0, 0)), CellKind::HeaderCorner);
        assert_eq!(table.cell_kind(CellRef::new(0, 1)), CellKind::HeaderRow);
        assert_eq!(table.cell_kind(CellRef::new(1, 0)), CellKind::HeaderColumn);
        assert_eq!(table.cell_kind(CellRef::new(1, 1)), CellKind::Body);
    }

    /// Header row over header column at the corner: a table with both reads down its
    /// first column as labels, so the corner belongs to the row.
    #[test]
    fn at_the_corner_the_header_row_wins() {
        let mut table = Table::new(2, 2);
        table.set_header_rows(1);
        table.set_header_columns(1);
        table.style_mut().header_column.fill = Some(Rgba::opaque(1, 1, 1));
        table.style_mut().header_row.fill = Some(Rgba::opaque(2, 2, 2));
        assert_eq!(table.resolved_style(CellRef::new(0, 0)).fill, Some(Rgba::opaque(2, 2, 2)));
        assert_eq!(table.resolved_style(CellRef::new(1, 0)).fill, Some(Rgba::opaque(1, 1, 1)));
    }

    #[test]
    fn an_interactive_resize_pins_a_proportional_column() {
        let mut table = Table::new(1, 2);
        table.set_column_sizing(0, Sizing::Proportional { weight: 1.0, min: 0.0 }).unwrap();
        table.resize_column_to(0, 140.0).unwrap();
        assert_eq!(table.columns()[0].sizing, Sizing::Fixed(140.0));
        table.resize_column_to(0, -50.0).unwrap();
        assert_eq!(table.columns()[0].sizing, Sizing::Fixed(MIN_TRACK_SIZE));
    }

    #[test]
    fn out_of_bounds_track_access_is_an_error_not_a_panic() {
        let mut table = Table::new(1, 1);
        assert_eq!(
            table.set_row_sizing(4, Sizing::Fixed(10.0)),
            Err(TableError::RowOutOfBounds { index: 4, count: 1 })
        );
        assert_eq!(
            table.set_column_sizing(4, Sizing::Fixed(10.0)),
            Err(TableError::ColumnOutOfBounds { index: 4, count: 1 })
        );
    }

    #[test]
    fn styling_survives_a_round_trip_through_json() {
        let mut table = Table::new(2, 2);
        table.set_header_rows(1);
        table.set_content(CellRef::new(1, 1), "value").unwrap();
        table.row_mut(1).unwrap().style.vertical_align = Some(VerticalAlign::Middle);
        table.merge(CellRange::new(0, 0, 1, 2), MergeContent::default()).unwrap();

        let json = serde_json::to_string(&table).unwrap();
        let back: Table = serde_json::from_str(&json).unwrap();
        assert_eq!(back, table);
        back.validate().unwrap();
    }
}
