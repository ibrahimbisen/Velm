//! File-tree nodes on the board: the document token, and the row geometry.
//!
//! `vellum_agent::filetree` reads the directory, one level at a time and lazily. This
//! module owns the token and turns a list of visible rows into rectangles — which is the
//! measurement that crate deliberately does not do, exactly as `vellum-flow` never measures
//! a kanban card.
//!
//! # Rows are addressed by index, and that is a trap worth naming
//!
//! The painter draws row *n* at a rectangle derived from *n*, and the press path resolves a
//! point back to *n*. Both use [`row_at`] and [`row_rect`], which are inverses of each
//! other and tested as such. Expanding a directory renumbers every row below it — the same
//! shape as the kanban caret's stale slot (feedback: "a stale slot draws the caret on one
//! card while typing into another") — so a caller holding a row index across an expand must
//! re-derive it rather than keep it.

use vellum_agent::FileTreeModel;

pub use crate::agent::Rect;

/// The token stored in the document for a file-tree node.
pub fn encode(model: &FileTreeModel) -> String {
    serde_json::to_string(model).unwrap_or_else(|error| {
        log::warn!("a file tree would not encode ({error}); storing nothing");
        String::new()
    })
}

/// The tree a token names, or a default one when it cannot be read. Never fails.
pub fn decode(token: &str) -> FileTreeModel {
    serde_json::from_str(token).unwrap_or_else(|error| {
        if !token.is_empty() {
            log::warn!("unreadable file tree ({error}); showing the project root");
        }
        FileTreeModel::default()
    })
}

/// What the file-tree tool places. Tall and narrow, like an editor's sidebar.
pub const DEFAULT_SIZE: (f64, f64) = (260.0, 420.0);

pub const MIN_SIZE: (f64, f64) = (120.0, 60.0);

const PAD: f64 = 10.0;
const HEADER_HEIGHT: f64 = 24.0;

/// One row's height, in world units. The 13px body size plus breathing room.
pub const ROW_HEIGHT: f64 = 20.0;

/// How far each level of nesting indents.
pub const INDENT: f64 = 14.0;

/// The disclosure triangle's box at the head of a directory row.
pub const TWISTY: f64 = 12.0;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TreeLayout {
    pub bounds: Rect,
    /// The root's name and the ignored-files toggle.
    pub header: Rect,
    /// The scrolling list of rows.
    pub list: Rect,
    pub too_small: bool,
}

impl TreeLayout {
    /// How many rows fit. The painter shapes only these — a `target/` directory with 40,000
    /// entries must cost what is on screen, not what exists, which is the same rule the
    /// whole canvas is built on.
    pub fn visible_rows(&self) -> usize {
        if self.list.height <= 0.0 {
            return 0;
        }
        (self.list.height / ROW_HEIGHT).floor().max(0.0) as usize
    }
}

pub fn layout(width: f64, height: f64) -> TreeLayout {
    let bounds = Rect::new(0.0, 0.0, width.max(0.0), height.max(0.0));
    if width < MIN_SIZE.0 || height < MIN_SIZE.1 {
        return TreeLayout { bounds, header: bounds, list: Rect::default(), too_small: true };
    }
    let inner = bounds.inset(PAD);
    let header = Rect::new(inner.x, inner.y, inner.width, HEADER_HEIGHT);
    let top = header.y + header.height;
    let list = Rect::new(inner.x, top, inner.width, (inner.y + inner.height - top).max(0.0));
    TreeLayout { bounds, header, list, too_small: false }
}

/// The rectangle of the `index`th visible row, given the list area and how far it is
/// scrolled (in whole rows).
pub fn row_rect(list: Rect, index: usize, scroll: usize) -> Rect {
    let offset = (index as f64 - scroll as f64) * ROW_HEIGHT;
    Rect::new(list.x, list.y + offset, list.width, ROW_HEIGHT)
}

/// Which visible row a point in item space is over, if any.
///
/// The exact inverse of [`row_rect`], and tested as such — a press path that reproduced the
/// arithmetic instead of sharing it is a click that lands on the row above the one under
/// the pointer.
pub fn row_at(list: Rect, y: f64, scroll: usize) -> Option<usize> {
    if list.height <= 0.0 || y < list.y || y >= list.y + list.height {
        return None;
    }
    let row = ((y - list.y) / ROW_HEIGHT).floor();
    if row < 0.0 {
        return None;
    }
    Some(scroll + row as usize)
}

/// Where a row's disclosure triangle sits, given the row's rectangle and its depth.
pub fn twisty_rect(row: Rect, depth: usize) -> Rect {
    let x = row.x + depth as f64 * INDENT;
    Rect::new(x, row.y + (row.height - TWISTY) / 2.0, TWISTY, TWISTY)
}

/// Where a row's label starts, given its depth.
pub fn label_x(row: Rect, depth: usize) -> f64 {
    row.x + depth as f64 * INDENT + TWISTY + 4.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unreadable_token_degrades_to_the_project_root() {
        assert_eq!(decode("{{{"), FileTreeModel::default());
        let tree = FileTreeModel { root: "src".into(), ..FileTreeModel::default() };
        assert_eq!(decode(&encode(&tree)), tree);
    }

    /// The two must be inverses. A press path that reproduced the arithmetic rather than
    /// sharing it is a click landing on the row above the one under the pointer — and it
    /// would be invisible in any test that only checked one direction.
    #[test]
    fn row_geometry_and_row_hit_testing_are_inverses() {
        let l = layout(DEFAULT_SIZE.0, DEFAULT_SIZE.1);
        for scroll in [0, 3, 40] {
            for index in scroll..scroll + l.visible_rows() {
                let rect = row_rect(l.list, index, scroll);
                let middle = rect.y + rect.height / 2.0;
                assert_eq!(
                    row_at(l.list, middle, scroll),
                    Some(index),
                    "row {index} at scroll {scroll} did not round-trip"
                );
                // And the very top of the row belongs to that row, not the one above.
                assert_eq!(row_at(l.list, rect.y + 0.01, scroll), Some(index));
            }
        }
    }

    #[test]
    fn a_point_outside_the_list_is_not_a_row() {
        let l = layout(DEFAULT_SIZE.0, DEFAULT_SIZE.1);
        assert_eq!(row_at(l.list, l.list.y - 1.0, 0), None, "the header answered as a row");
        assert_eq!(row_at(l.list, l.list.y + l.list.height + 1.0, 0), None);
        assert_eq!(row_at(Rect::default(), 0.0, 0), None);
    }

    /// Only what is on screen may be shaped. The whole canvas rests on this rule, and a
    /// file tree is the one node type where the underlying list is routinely enormous.
    #[test]
    fn only_the_rows_that_fit_are_reported_as_visible() {
        let l = layout(DEFAULT_SIZE.0, DEFAULT_SIZE.1);
        let fits = l.visible_rows();
        assert!(fits > 0);
        assert!(
            fits as f64 * ROW_HEIGHT <= l.list.height + 0.001,
            "{fits} rows do not fit in {}",
            l.list.height
        );
        assert!((fits + 1) as f64 * ROW_HEIGHT > l.list.height, "one more row would have fitted");

        assert_eq!(layout(40.0, 30.0).visible_rows(), 0, "a compact tree offered rows");
    }

    #[test]
    fn nesting_indents_the_twisty_and_the_label_together() {
        let l = layout(DEFAULT_SIZE.0, DEFAULT_SIZE.1);
        let row = row_rect(l.list, 0, 0);
        let (shallow, deep) = (twisty_rect(row, 0), twisty_rect(row, 2));
        assert_eq!(deep.x - shallow.x, INDENT * 2.0);
        assert!(label_x(row, 0) > shallow.x + TWISTY, "a label sat on its own twisty");
        assert_eq!(label_x(row, 2) - label_x(row, 0), INDENT * 2.0);
    }
}
