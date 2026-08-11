//! What can go wrong, and what deliberately cannot.
//!
//! Two classes of operation live in this crate and they fail differently.
//!
//! **Structural edits are fallible.** Merging, unmerging, inserting and deleting all
//! take indices, and an index that came from a stale selection or a keyboard
//! shortcut on an empty table has to be rejected rather than clamped. Clamping would
//! turn "delete row 7" on a 4-row table into "delete row 3", which is a silent data
//! loss.
//!
//! **Styling and sizing are total.** A nonsense colour, a negative width, an absurd
//! font size — these clamp, exactly as `vellum-shapes` clamps its shape parameters,
//! and for the same reason: a bad style should look wrong on the board, not abort a
//! board load. Every length passes through one clamp in [`geometry`](crate::geometry)
//! that turns a negative, infinite or `NaN` value into zero.
//!
//! [`TableError::Invariant`] is the odd one out. It is never produced by an
//! operation, only by [`Table::validate`](crate::Table::validate), and it exists so
//! that the merge invariants are a *testable claim* rather than a comment. Every
//! structural test in this crate ends by validating.

use thiserror::Error;

use crate::grid::CellRef;

pub type Result<T> = std::result::Result<T, TableError>;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum TableError {
    #[error("row {index} is out of bounds: the table has {count} rows")]
    RowOutOfBounds { index: usize, count: usize },

    #[error("column {index} is out of bounds: the table has {count} columns")]
    ColumnOutOfBounds { index: usize, count: usize },

    /// A range covering no cells. Rejected rather than treated as a no-op because
    /// every caller reached it from a selection, and an empty selection means the
    /// caller's own state is wrong.
    #[error("the range at ({row}, {col}) covers no cells: {rows} rows × {cols} columns")]
    EmptyRange { row: usize, col: usize, rows: usize, cols: usize },

    /// The target is inside a merged region rather than being its top-left cell.
    /// Callers resolve it with
    /// [`Table::anchor_at`](crate::Table::anchor_at) first.
    #[error(
        "({row}, {col}) is covered by the merged cell anchored at ({anchor_row}, {anchor_col})"
    )]
    NotAnAnchor { row: usize, col: usize, anchor_row: usize, anchor_col: usize },

    /// A merge invariant does not hold. Only [`Table::validate`](crate::Table::validate)
    /// returns this; seeing it anywhere else is a bug in this crate.
    #[error("grid invariant violated at ({row}, {col}): {detail}")]
    Invariant { row: usize, col: usize, detail: &'static str },
}

impl TableError {
    pub(crate) fn invariant(at: CellRef, detail: &'static str) -> Self {
        Self::Invariant { row: at.row, col: at.col, detail }
    }

    pub(crate) fn not_an_anchor(at: CellRef, anchor: CellRef) -> Self {
        Self::NotAnAnchor {
            row: at.row,
            col: at.col,
            anchor_row: anchor.row,
            anchor_col: anchor.col,
        }
    }
}
