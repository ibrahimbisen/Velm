//! File-tree nodes on the board: the document token, and the row geometry.
//!
//! `vellum_agent::filetree` reads the directory, one level at a time and lazily. This
//! module owns the token and turns a list of visible rows into rectangles — which is the
//! measurement that crate deliberately does not do, exactly as `vellum-flow` never measures
//! a kanban card.
//!
//! # A row index is not what the press path resolves against, and that is deliberate
//!
//! [`row_rect`] is where a row is drawn. The press path does **not** invert it: it reads
//! `crate::draw::NodePaint::tree_rows`, the rectangles the painter actually painted, because
//! three separate things decide which row ends up where — `TreeLayout::visible_rows`, the row
//! the painter holds back for its *"n more"* line, and the scroll offset. An inverse that knew
//! only the arithmetic would resolve a point to a row that is not on the screen.
//!
//! There *was* an inverse here, `row_at`, written and tested and called by nothing — which is
//! how a tree came to be drawn with disclosure triangles that could not be pressed at all. It
//! is gone rather than left as a second answer to a question `NodePaint` already answers.
//!
//! Expanding a directory renumbers every row below it — the same shape as the kanban caret's
//! stale slot (feedback: "a stale slot draws the caret on one card while typing into
//! another") — which is why the press path carries a row's **relative path** away with it and
//! never an index.

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

    /// Rows stack without a gap and without an overlap. A press lands on exactly one of them
    /// because the painter records what it drew — so what this has to guarantee is that the
    /// rectangles themselves tile, not that some inverse agrees with them.
    #[test]
    fn rows_tile_the_list_without_gaps_or_overlaps() {
        let l = layout(DEFAULT_SIZE.0, DEFAULT_SIZE.1);
        let fits = l.visible_rows();
        assert!(fits > 1);
        for index in 0..fits - 1 {
            let (row, next) = (row_rect(l.list, index, 0), row_rect(l.list, index + 1, 0));
            assert!((next.y - (row.y + row.height)).abs() < 1e-9, "row {index} left a seam");
            assert_eq!(row.x, l.list.x);
            assert_eq!(row.width, l.list.width);
        }
        assert_eq!(row_rect(l.list, 0, 0).y, l.list.y, "the first row missed the top");
        // A scrolled list draws the same rectangles for the rows it is showing, which is what
        // lets the offset be a display concern rather than a geometry one.
        assert_eq!(row_rect(l.list, 3, 3), row_rect(l.list, 0, 0));
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
