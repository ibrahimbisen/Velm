//! The one thing in this crate that can fail.
//!
//! Shape construction itself is total: every parameter is clamped to a range that
//! still describes a drawable shape, because a shape with a nonsense corner radius
//! should look wrong on the board, not abort a board load. Only tessellation can
//! fail, and only for reasons internal to `lyon`.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ShapeError {
    #[error("tessellation failed: {0}")]
    Tessellation(#[from] lyon::tessellation::TessellationError),
}
