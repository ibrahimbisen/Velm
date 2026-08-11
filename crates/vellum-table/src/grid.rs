//! The merge-aware grid: cells, spans, and the four structural edits.
//!
//! # The representation, and why it is dense
//!
//! The grid is a **row-major `Vec` with one slot per grid position**, always
//! `rows × columns` long. A slot is either an [`Slot::Anchor`] — a real cell, whose
//! [`GridSpan`] says how far right and down it reaches — or [`Slot::Covered`],
//! naming the anchor that occupies it.
//!
//! The obvious alternative is a sparse list of cells with `(row, col, rowspan,
//! colspan)`, as HTML's DOM effectively has. It is rejected because the two
//! questions this crate is asked most often are *"which cell is under this point"*
//! and *"which cell is at (r, c)"*, and the sparse form answers both by scanning
//! every cell and testing containment. Layout, hit-testing and border generation all
//! do that per position, which turns each of them quadratic in cell count for a
//! structure that is at most a few hundred cells. Dense makes them `O(1)` lookups
//! at the cost of one `usize` pair per covered position.
//!
//! # The invariants
//!
//! Four claims hold after **every** operation here, and
//! [`Grid::validate`] checks all four:
//!
//! 1. `slots.len() == rows * columns`.
//! 2. Every anchor's span is at least `1×1` and stays inside the grid.
//! 3. Every position inside an anchor's rectangle, other than its top-left, is
//!    `Covered` **by that anchor** — which is what forbids two merges overlapping,
//!    since a shared position cannot name two different anchors.
//! 4. Every `Covered` names a position that really is an anchor and whose rectangle
//!    really does reach it — no orphans.
//!
//! Invariant 4 is the one that a naive delete breaks: removing the top row of a
//! vertical merge leaves every position below it pointing at a cell that no longer
//! exists. [`Grid::remove_row`] handles that by **re-anchoring** — the merged cell
//! keeps its [`CellId`] and moves down into the row that survives — rather than by
//! deleting the merge, so the content and the identity both survive and the
//! measurement cache stays warm.
//!
//! # Why structural edits end with a restamp
//!
//! Each of the four edits shifts every slot after the insertion point, so a
//! `Covered` that stored an absolute anchor position would be wrong for most of the
//! grid afterwards. Rather than patching those references — which is where an
//! implementation like this normally goes wrong — every edit adjusts the *spans*,
//! which are relative, and then rebuilds every `Covered` from the anchors in one
//! pass. That pass is `O(rows × columns)`, which is already the cost of the `Vec`
//! shift the edit performed anyway, and it means invariants 3 and 4 hold by
//! construction rather than by argument.

use serde::{Deserialize, Serialize};

use crate::error::{Result, TableError};
use crate::span::StyledText;
use crate::style::CellStyle;

/// A cell's stable identity, unique within one table for the table's lifetime.
///
/// Positions are not identity: a cell moves when a row is inserted above it, and a
/// merged cell re-anchors when its top row is deleted. The measurement cache is
/// keyed on this, so a cell that merely moved keeps its cached intrinsic widths —
/// without which inserting a row at the top of a table would re-measure every cell
/// below it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct CellId(pub u64);

/// A grid position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct CellRef {
    pub row: usize,
    pub col: usize,
}

impl CellRef {
    pub const fn new(row: usize, col: usize) -> Self {
        Self { row, col }
    }
}

/// How far a merged cell reaches. `1×1` is an ordinary cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct GridSpan {
    pub rows: usize,
    pub cols: usize,
}

impl GridSpan {
    pub const SINGLE: Self = Self { rows: 1, cols: 1 };

    pub const fn new(rows: usize, cols: usize) -> Self {
        Self { rows, cols }
    }

    pub fn is_single(self) -> bool {
        self.rows <= 1 && self.cols <= 1
    }
}

impl Default for GridSpan {
    fn default() -> Self {
        Self::SINGLE
    }
}

/// A rectangular block of grid positions — a selection, or the target of a merge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CellRange {
    pub row: usize,
    pub col: usize,
    pub rows: usize,
    pub cols: usize,
}

impl CellRange {
    pub const fn new(row: usize, col: usize, rows: usize, cols: usize) -> Self {
        Self { row, col, rows, cols }
    }

    /// The block spanned by two corners, in either order — which is what a drag
    /// selection produces, since the user may drag up and to the left.
    pub fn from_corners(a: CellRef, b: CellRef) -> Self {
        let row = a.row.min(b.row);
        let col = a.col.min(b.col);
        Self::new(row, col, a.row.abs_diff(b.row) + 1, a.col.abs_diff(b.col) + 1)
    }

    pub fn single(at: CellRef) -> Self {
        Self::new(at.row, at.col, 1, 1)
    }

    /// The block a cell occupies.
    pub fn of(anchor: CellRef, span: GridSpan) -> Self {
        Self::new(anchor.row, anchor.col, span.rows, span.cols)
    }

    /// One past the last row, in the half-open convention the rest of the crate
    /// uses.
    pub fn end_row(self) -> usize {
        self.row + self.rows
    }

    pub fn end_col(self) -> usize {
        self.col + self.cols
    }

    pub fn is_empty(self) -> bool {
        self.rows == 0 || self.cols == 0
    }

    pub fn span(self) -> GridSpan {
        GridSpan::new(self.rows, self.cols)
    }

    pub fn anchor(self) -> CellRef {
        CellRef::new(self.row, self.col)
    }

    pub fn contains(self, at: CellRef) -> bool {
        at.row >= self.row
            && at.row < self.end_row()
            && at.col >= self.col
            && at.col < self.end_col()
    }

    /// The smallest block containing both. Empty ranges are absorbed rather than
    /// contributing a phantom corner at their origin.
    pub fn union(self, other: Self) -> Self {
        if self.is_empty() {
            return other;
        }
        if other.is_empty() {
            return self;
        }
        let row = self.row.min(other.row);
        let col = self.col.min(other.col);
        let end_row = self.end_row().max(other.end_row());
        let end_col = self.end_col().max(other.end_col());
        Self::new(row, col, end_row - row, end_col - col)
    }

    /// Every position in the block, row-major.
    pub fn cells(self) -> impl Iterator<Item = CellRef> {
        (self.row..self.end_row())
            .flat_map(move |r| (self.col..self.end_col()).map(move |c| CellRef::new(r, c)))
    }
}

/// The style a cell with no overrides of its own reports. See [`Cell::style`].
static NO_OVERRIDES: CellStyle = CellStyle::EMPTY;

/// A cell: its text, its style overrides, and how far it spans.
///
/// The fields are private because two of them have obligations. Mutating
/// [`content`](Self::content) or [`style`](Self::style) must bump
/// [`revision`](Self::revision), or the measurement cache will serve a stale size
/// for edited text; and [`span`](Self::span) may only be changed by the grid, which
/// is the only thing that can keep the covered positions consistent with it.
///
/// **The style override is boxed and usually absent.** A `CellStyle` is ten options
/// and four borders — around 200 bytes — and the cascade exists precisely so that
/// almost no cell needs one. Inline, it would triple the size of every [`Slot`] in
/// a representation whose whole argument is that a dense grid is cheap; boxed, a
/// cell is 64 bytes and the allocation happens only for the cells a user has
/// actually restyled.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cell {
    id: CellId,
    content: StyledText,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    style: Option<Box<CellStyle>>,
    span: GridSpan,
    revision: u64,
}

/// Compares what a cell *is*, not how it is stored.
///
/// Two things are deliberately outside the comparison. A style override that has
/// been materialised but left empty is the same cell as one that never had one — so
/// `style_mut` cannot change a cell's identity by being called. And the revision is
/// bookkeeping for the measurement cache, so a cell edited from "a" to "b" and back
/// equals one that was never touched, which is what makes an undo test mean
/// anything.
impl PartialEq for Cell {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
            && self.span == other.span
            && self.content == other.content
            && self.style() == other.style()
    }
}

impl Cell {
    pub(crate) fn new(id: CellId) -> Self {
        Self {
            id,
            content: StyledText::default(),
            style: None,
            span: GridSpan::SINGLE,
            revision: 0,
        }
    }

    pub fn id(&self) -> CellId {
        self.id
    }

    pub fn content(&self) -> &StyledText {
        &self.content
    }

    /// This cell's own overrides — [`CellStyle::EMPTY`] when it has none.
    pub fn style(&self) -> &CellStyle {
        self.style.as_deref().unwrap_or(&NO_OVERRIDES)
    }

    pub fn span(&self) -> GridSpan {
        self.span
    }

    /// Bumped on every edit that can change how the cell measures. The measurement
    /// cache keys on it, so a missed bump shows up as text that changes without the
    /// row growing to fit it.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn set_content(&mut self, content: impl Into<StyledText>) {
        self.content = content.into();
        self.revision = self.revision.wrapping_add(1);
    }

    /// Replaces this cell's overrides. An empty style is stored as *no* override, so
    /// clearing one costs nothing to keep.
    pub fn set_style(&mut self, style: CellStyle) {
        self.style = (!style.is_empty()).then(|| Box::new(style));
        self.revision = self.revision.wrapping_add(1);
    }

    pub fn clear_style(&mut self) {
        self.style = None;
        self.revision = self.revision.wrapping_add(1);
    }

    /// Mutable access to the overrides, materialising them if the cell had none, and
    /// bumping the revision **on access** rather than on change.
    ///
    /// Pessimistic on purpose: the alternative is comparing the style before and
    /// after, and a style that compares equal after a caller has taken a `&mut` to
    /// it is not worth a `PartialEq` over ten fields. The cost of being wrong here
    /// is one re-measure of one cell.
    pub fn style_mut(&mut self) -> &mut CellStyle {
        self.revision = self.revision.wrapping_add(1);
        self.style.get_or_insert_with(|| Box::new(CellStyle::EMPTY))
    }

    pub(crate) fn set_span(&mut self, span: GridSpan) {
        self.span = span;
    }
}

/// One grid position.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Slot {
    /// A real cell. Its top-left corner is this position.
    Anchor(Cell),
    /// Occupied by the merged cell anchored at the named position.
    Covered(CellRef),
}

/// What happens to the text of the cells a merge swallows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum MergeContent {
    /// Join every non-empty cell's text, row-major, separated by a newline.
    ///
    /// The default. Merging is a *formatting* gesture — the user is drawing a table,
    /// not editing text — and a formatting gesture that silently deletes three cells
    /// of typing is the kind of thing that gets noticed a week later, when undo is
    /// long gone.
    #[default]
    Concatenate,
    /// Keep the top-left cell's text and discard the rest. What Sheets does, and
    /// what a caller that has already asked the user for confirmation wants.
    KeepAnchor,
}

/// The merge-aware cell store. See the module docs for the representation and the
/// invariants.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Grid {
    rows: usize,
    cols: usize,
    slots: Vec<Slot>,
    next_id: u64,
}

impl Grid {
    pub fn new(rows: usize, cols: usize) -> Self {
        let mut grid = Self { rows, cols, slots: Vec::with_capacity(rows * cols), next_id: 0 };
        for _ in 0..rows * cols {
            let cell = grid.fresh_cell();
            grid.slots.push(Slot::Anchor(cell));
        }
        grid
    }

    pub fn rows(&self) -> usize {
        self.rows
    }

    pub fn columns(&self) -> usize {
        self.cols
    }

    /// True when the grid encloses no positions. Both a `0 × 4` and a `4 × 0` table
    /// are legal and reachable — deleting the last row of a table leaves one — and
    /// everything downstream has to cope rather than assume.
    pub fn is_empty(&self) -> bool {
        self.rows == 0 || self.cols == 0
    }

    fn fresh_cell(&mut self) -> Cell {
        let id = CellId(self.next_id);
        self.next_id += 1;
        Cell::new(id)
    }

    fn in_bounds(&self, at: CellRef) -> bool {
        at.row < self.rows && at.col < self.cols
    }

    fn index_of(&self, at: CellRef) -> usize {
        at.row * self.cols + at.col
    }

    fn ref_at(&self, index: usize) -> CellRef {
        // `cols == 0` implies no slots, so no index can reach this; the guard keeps
        // the division total anyway rather than relying on that argument.
        if self.cols == 0 {
            return CellRef::new(0, 0);
        }
        CellRef::new(index / self.cols, index % self.cols)
    }

    pub fn slot(&self, at: CellRef) -> Option<&Slot> {
        if self.in_bounds(at) { self.slots.get(self.index_of(at)) } else { None }
    }

    /// The cell anchored exactly here, or `None` if the position is covered or out
    /// of bounds.
    pub fn cell(&self, at: CellRef) -> Option<&Cell> {
        match self.slot(at) {
            Some(Slot::Anchor(cell)) => Some(cell),
            _ => None,
        }
    }

    /// Where the cell occupying this position is anchored — itself, for an ordinary
    /// cell.
    pub fn anchor_at(&self, at: CellRef) -> Option<CellRef> {
        match self.slot(at)? {
            Slot::Anchor(_) => Some(at),
            Slot::Covered(anchor) => Some(*anchor),
        }
    }

    /// The cell occupying this position, resolving through a merge.
    pub fn cell_at(&self, at: CellRef) -> Option<(CellRef, &Cell)> {
        let anchor = self.anchor_at(at)?;
        self.cell(anchor).map(|cell| (anchor, cell))
    }

    /// Mutable access to the cell anchored here. Covered positions are rejected
    /// rather than redirected: a caller writing to a covered position has stale
    /// coordinates, and silently redirecting the write would put the text somewhere
    /// the user did not click.
    pub fn cell_mut(&mut self, at: CellRef) -> Result<&mut Cell> {
        self.require_anchor(at)?;
        let index = self.index_of(at);
        match &mut self.slots[index] {
            Slot::Anchor(cell) => Ok(cell),
            Slot::Covered(_) => unreachable!("require_anchor just checked this"),
        }
    }

    fn require_anchor(&self, at: CellRef) -> Result<()> {
        if at.row >= self.rows {
            return Err(TableError::RowOutOfBounds { index: at.row, count: self.rows });
        }
        if at.col >= self.cols {
            return Err(TableError::ColumnOutOfBounds { index: at.col, count: self.cols });
        }
        match &self.slots[self.index_of(at)] {
            Slot::Anchor(_) => Ok(()),
            Slot::Covered(anchor) => Err(TableError::not_an_anchor(at, *anchor)),
        }
    }

    /// Every real cell, row-major. Covered positions are not visited, so this is
    /// exactly the set of cells that get laid out and drawn.
    pub fn anchors(&self) -> impl Iterator<Item = (CellRef, &Cell)> {
        self.slots.iter().enumerate().filter_map(|(i, slot)| match slot {
            Slot::Anchor(cell) => Some((self.ref_at(i), cell)),
            Slot::Covered(_) => None,
        })
    }

    /// Number of real cells — at most `rows × columns`, fewer once anything is
    /// merged.
    pub fn cell_count(&self) -> usize {
        self.slots.iter().filter(|s| matches!(s, Slot::Anchor(_))).count()
    }

    /// Adjusts an anchor's span in place, mid-edit.
    ///
    /// Total rather than fallible on purpose. Every caller collected the position
    /// from [`anchors`](Self::anchors) moments earlier and has already moved slots
    /// around, so there is no state left to return an error *from* — a `?` here
    /// would abandon a half-applied structural edit, which is worse than the
    /// impossible case it guards.
    fn map_span(&mut self, at: CellRef, f: impl FnOnce(&mut GridSpan)) {
        let index = self.index_of(at);
        if let Some(Slot::Anchor(cell)) = self.slots.get_mut(index) {
            f(&mut cell.span);
        }
    }

    fn check_range(&self, range: CellRange) -> Result<()> {
        if range.is_empty() {
            return Err(TableError::EmptyRange {
                row: range.row,
                col: range.col,
                rows: range.rows,
                cols: range.cols,
            });
        }
        if range.end_row() > self.rows {
            return Err(TableError::RowOutOfBounds {
                index: range.end_row() - 1,
                count: self.rows,
            });
        }
        if range.end_col() > self.cols {
            return Err(TableError::ColumnOutOfBounds {
                index: range.end_col() - 1,
                count: self.cols,
            });
        }
        Ok(())
    }

    /// Grows a range until it contains every merged cell it touches.
    ///
    /// A range that clips half of an existing merge cannot be merged as asked —
    /// the result would be a non-rectangular cell. Every table editor that allows
    /// the gesture at all resolves it the same way, by swallowing the whole merge,
    /// and so does this: the user's selection is a statement about the region, not
    /// about the exact indices.
    ///
    /// Terminates because each pass either grows the range — bounded by the grid —
    /// or stops.
    fn expanded_over_merges(&self, range: CellRange) -> CellRange {
        let mut range = range;
        loop {
            let mut grown = range;
            for at in range.cells() {
                if let Some((anchor, cell)) = self.cell_at(at) {
                    grown = grown.union(CellRange::of(anchor, cell.span));
                }
            }
            if grown == range {
                return range;
            }
            range = grown;
        }
    }

    /// Merges a range into one cell, returning the range actually merged — which is
    /// the requested one grown over any merge it clipped.
    ///
    /// The surviving cell keeps the **top-left cell's identity and style**; only its
    /// text is affected, per `policy`.
    pub fn merge(&mut self, range: CellRange, policy: MergeContent) -> Result<CellRange> {
        self.check_range(range)?;
        let range = self.expanded_over_merges(range);
        let anchor = range.anchor();

        // After expansion the top-left is necessarily an anchor: had it been
        // covered, expansion would have pulled its anchor — which lies above and to
        // the left — into the range, moving the top-left corner there. Checked
        // rather than asserted, and checked *before* anything is written: the only
        // way this can fail is a grid that was already corrupt, and a half-applied
        // merge on top of that would destroy the evidence.
        self.require_anchor(anchor)?;

        let content = match policy {
            MergeContent::KeepAnchor => None,
            MergeContent::Concatenate => {
                let parts: Vec<StyledText> = range
                    .cells()
                    .filter_map(|at| self.cell(at))
                    .map(|cell| cell.content.clone())
                    .collect();
                Some(StyledText::concat(parts, "\n"))
            }
        };

        for at in range.cells() {
            let index = self.index_of(at);
            if at == anchor {
                continue;
            }
            self.slots[index] = Slot::Covered(anchor);
        }

        let index = self.index_of(anchor);
        if let Slot::Anchor(cell) = &mut self.slots[index] {
            cell.span = range.span();
            if let Some(content) = content {
                cell.set_content(content);
            }
        }
        Ok(range)
    }

    /// Splits a merged cell back into ordinary ones.
    ///
    /// The text stays on the top-left cell rather than being redistributed. There is
    /// no information in the merged value about which cell any of it came from, and
    /// guessing — splitting on newlines, say — would be wrong for any cell whose
    /// text genuinely contained one.
    pub fn unmerge(&mut self, at: CellRef) -> Result<()> {
        self.require_anchor(at)?;
        let span = self.cell(at).expect("checked").span;
        if span.is_single() {
            return Ok(());
        }
        self.cell_mut(at)?.set_span(GridSpan::SINGLE);
        for freed in CellRange::of(at, span).cells() {
            if freed == at {
                continue;
            }
            let cell = self.fresh_cell();
            let index = self.index_of(freed);
            self.slots[index] = Slot::Anchor(cell);
        }
        Ok(())
    }

    /// Inserts an empty row, pushing rows at and below `index` down.
    ///
    /// A merge that *straddles* the insertion point grows to keep covering the same
    /// cells; a merge that merely *touches* it at its top or bottom edge does not.
    /// Inserting at a merge's own top row therefore puts the new row above it, which
    /// is the only reading of "insert above this cell" that leaves the merge where
    /// the user can still see it.
    pub fn insert_row(&mut self, index: usize) -> Result<()> {
        if index > self.rows {
            return Err(TableError::RowOutOfBounds { index, count: self.rows });
        }
        let straddling: Vec<CellRef> = self
            .anchors()
            .filter(|(at, cell)| at.row < index && index < at.row + cell.span.rows)
            .map(|(at, _)| at)
            .collect();

        let mut fresh = Vec::with_capacity(self.cols);
        for _ in 0..self.cols {
            let cell = self.fresh_cell();
            fresh.push(Slot::Anchor(cell));
        }
        let at = index * self.cols;
        self.slots.splice(at..at, fresh);
        self.rows += 1;

        // Straddling anchors are above `index`, so their positions did not move.
        for anchor in straddling {
            self.map_span(anchor, |span| span.rows += 1);
        }
        self.restamp();
        Ok(())
    }

    /// Removes a row.
    ///
    /// The case that makes this interesting is a merge anchored *on* the removed
    /// row and spanning past it: deleting the anchor would orphan every position
    /// below. Instead the cell is **re-anchored** one row down, keeping its
    /// [`CellId`], its text and its style, with its span reduced by one. A merge
    /// that fitted entirely within the row goes with it.
    pub fn remove_row(&mut self, index: usize) -> Result<()> {
        if index >= self.rows {
            return Err(TableError::RowOutOfBounds { index, count: self.rows });
        }

        let mut re_anchored: Vec<(CellRef, Cell)> = Vec::new();
        let mut shrinking: Vec<CellRef> = Vec::new();
        for (at, cell) in self.anchors() {
            if at.row == index && cell.span.rows > 1 {
                let mut moved = cell.clone();
                moved.span.rows -= 1;
                re_anchored.push((CellRef::new(at.row + 1, at.col), moved));
            } else if at.row < index && index < at.row + cell.span.rows {
                shrinking.push(at);
            }
        }
        for (to, cell) in re_anchored {
            let target = self.index_of(to);
            self.slots[target] = Slot::Anchor(cell);
        }
        for anchor in shrinking {
            self.map_span(anchor, |span| span.rows -= 1);
        }

        let at = index * self.cols;
        self.slots.drain(at..at + self.cols);
        self.rows -= 1;
        self.restamp();
        Ok(())
    }

    /// Inserts an empty column, mirroring [`insert_row`](Self::insert_row).
    pub fn insert_column(&mut self, index: usize) -> Result<()> {
        if index > self.cols {
            return Err(TableError::ColumnOutOfBounds { index, count: self.cols });
        }
        let straddling: Vec<CellRef> = self
            .anchors()
            .filter(|(at, cell)| at.col < index && index < at.col + cell.span.cols)
            .map(|(at, _)| at)
            .collect();

        let old = std::mem::take(&mut self.slots);
        let old_cols = self.cols;
        let mut rebuilt = Vec::with_capacity(self.rows * (old_cols + 1));
        let mut source = old.into_iter();
        for _ in 0..self.rows {
            for _ in 0..index {
                rebuilt.push(source.next().expect("row-major traversal is exhaustive"));
            }
            let cell = self.fresh_cell();
            rebuilt.push(Slot::Anchor(cell));
            for _ in index..old_cols {
                rebuilt.push(source.next().expect("row-major traversal is exhaustive"));
            }
        }
        self.slots = rebuilt;
        self.cols += 1;

        for anchor in straddling {
            self.map_span(anchor, |span| span.cols += 1);
        }
        self.restamp();
        Ok(())
    }

    /// Removes a column, mirroring [`remove_row`](Self::remove_row) — including the
    /// re-anchoring of a horizontal merge whose leftmost column is the one going
    /// away.
    pub fn remove_column(&mut self, index: usize) -> Result<()> {
        if index >= self.cols {
            return Err(TableError::ColumnOutOfBounds { index, count: self.cols });
        }

        let mut re_anchored: Vec<(CellRef, Cell)> = Vec::new();
        let mut shrinking: Vec<CellRef> = Vec::new();
        for (at, cell) in self.anchors() {
            if at.col == index && cell.span.cols > 1 {
                let mut moved = cell.clone();
                moved.span.cols -= 1;
                re_anchored.push((CellRef::new(at.row, at.col + 1), moved));
            } else if at.col < index && index < at.col + cell.span.cols {
                shrinking.push(at);
            }
        }
        for (to, cell) in re_anchored {
            let target = self.index_of(to);
            self.slots[target] = Slot::Anchor(cell);
        }
        for anchor in shrinking {
            self.map_span(anchor, |span| span.cols -= 1);
        }

        let old = std::mem::take(&mut self.slots);
        let old_cols = self.cols;
        let mut rebuilt = Vec::with_capacity(self.rows * (old_cols - 1));
        for (i, slot) in old.into_iter().enumerate() {
            if i % old_cols != index {
                rebuilt.push(slot);
            }
        }
        self.slots = rebuilt;
        self.cols -= 1;
        self.restamp();
        Ok(())
    }

    /// Clamps every span to the grid and rebuilds every covered position from the
    /// anchors. See the module docs for why every structural edit ends here.
    fn restamp(&mut self) {
        let (rows, cols) = (self.rows, self.cols);
        let mut anchors: Vec<(CellRef, GridSpan)> = Vec::new();
        for index in 0..self.slots.len() {
            let at = self.ref_at(index);
            if let Slot::Anchor(cell) = &mut self.slots[index] {
                cell.span.rows = cell.span.rows.clamp(1, rows - at.row);
                cell.span.cols = cell.span.cols.clamp(1, cols - at.col);
                anchors.push((at, cell.span));
            }
        }
        for (anchor, span) in anchors {
            for covered in CellRange::of(anchor, span).cells() {
                if covered == anchor {
                    continue;
                }
                let index = self.index_of(covered);
                debug_assert!(
                    !matches!(&self.slots[index], Slot::Anchor(cell) if !cell.span.is_single()),
                    "restamp would clobber the merged cell at {covered:?}: overlapping spans"
                );
                self.slots[index] = Slot::Covered(anchor);
            }
        }
    }

    /// Checks the four invariants in the module docs. Every structural test in this
    /// crate ends with a call to this, which is what makes them claims about the
    /// grid rather than about the one position they happened to look at.
    pub fn validate(&self) -> Result<()> {
        if self.slots.len() != self.rows * self.cols {
            return Err(TableError::invariant(
                CellRef::new(0, 0),
                "slot count does not match rows × columns",
            ));
        }
        let mut seen_ids: Vec<CellId> = Vec::with_capacity(self.slots.len());
        for (index, slot) in self.slots.iter().enumerate() {
            let at = self.ref_at(index);
            match slot {
                Slot::Anchor(cell) => {
                    seen_ids.push(cell.id);
                    if cell.span.rows == 0 || cell.span.cols == 0 {
                        return Err(TableError::invariant(at, "span is degenerate"));
                    }
                    if at.row + cell.span.rows > self.rows || at.col + cell.span.cols > self.cols {
                        return Err(TableError::invariant(at, "span reaches outside the grid"));
                    }
                    for covered in CellRange::of(at, cell.span).cells() {
                        if covered == at {
                            continue;
                        }
                        match &self.slots[self.index_of(covered)] {
                            Slot::Covered(owner) if *owner == at => {}
                            _ => {
                                return Err(TableError::invariant(
                                    covered,
                                    "position inside a span is not covered by that span's anchor",
                                ));
                            }
                        }
                    }
                }
                Slot::Covered(owner) => {
                    let Some(Slot::Anchor(cell)) = self.slot(*owner) else {
                        return Err(TableError::invariant(
                            at,
                            "covered by a position that is not an anchor",
                        ));
                    };
                    if !CellRange::of(*owner, cell.span).contains(at) {
                        return Err(TableError::invariant(
                            at,
                            "covered by a span that does not reach it",
                        ));
                    }
                }
            }
        }
        // Ids are what the measurement cache keys on, so a duplicate would serve one
        // cell's cached size for another's text. Only a bad clone can produce one,
        // which is exactly the mistake re-anchoring a merge invites.
        seen_ids.sort_unstable();
        if seen_ids.windows(2).any(|w| w[0] == w[1]) {
            return Err(TableError::invariant(CellRef::new(0, 0), "two cells share one id"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filled(rows: usize, cols: usize) -> Grid {
        let mut grid = Grid::new(rows, cols);
        for r in 0..rows {
            for c in 0..cols {
                grid.cell_mut(CellRef::new(r, c))
                    .unwrap()
                    .set_content(StyledText::plain(format!("{r}{c}")));
            }
        }
        grid
    }

    fn text_at(grid: &Grid, row: usize, col: usize) -> String {
        grid.cell(CellRef::new(row, col)).map(|c| c.content().to_plain()).unwrap_or_default()
    }

    #[test]
    fn a_fresh_grid_is_all_single_cells_with_distinct_ids() {
        let grid = Grid::new(3, 4);
        assert_eq!(grid.cell_count(), 12);
        assert!(grid.anchors().all(|(_, cell)| cell.span().is_single()));
        grid.validate().unwrap();
    }

    #[test]
    fn merging_covers_the_range_and_keeps_one_cell() {
        let mut grid = filled(3, 3);
        let merged = grid.merge(CellRange::new(0, 0, 2, 2), MergeContent::KeepAnchor).unwrap();
        assert_eq!(merged, CellRange::new(0, 0, 2, 2));
        assert_eq!(grid.cell_count(), 3 * 3 - 3);
        assert_eq!(grid.anchor_at(CellRef::new(1, 1)), Some(CellRef::new(0, 0)));
        assert_eq!(text_at(&grid, 0, 0), "00", "KeepAnchor discards the other three");
        grid.validate().unwrap();
    }

    #[test]
    fn the_default_merge_policy_keeps_every_cell_of_text() {
        let mut grid = filled(2, 2);
        grid.merge(CellRange::new(0, 0, 2, 2), MergeContent::default()).unwrap();
        assert_eq!(text_at(&grid, 0, 0), "00\n01\n10\n11");
        grid.validate().unwrap();
    }

    /// A range that clips a merge cannot be merged as asked without producing a
    /// non-rectangular cell, so it grows.
    #[test]
    fn a_merge_that_clips_another_swallows_it_whole() {
        let mut grid = filled(4, 4);
        grid.merge(CellRange::new(1, 1, 2, 2), MergeContent::KeepAnchor).unwrap();
        let merged = grid.merge(CellRange::new(0, 0, 2, 2), MergeContent::KeepAnchor).unwrap();
        assert_eq!(merged, CellRange::new(0, 0, 3, 3), "grew to contain the 2×2 at (1,1)");
        assert_eq!(grid.cell(CellRef::new(0, 0)).unwrap().span(), GridSpan::new(3, 3));
        grid.validate().unwrap();
    }

    #[test]
    fn unmerging_frees_the_covered_positions_as_fresh_cells() {
        let mut grid = filled(2, 3);
        grid.merge(CellRange::new(0, 0, 2, 2), MergeContent::KeepAnchor).unwrap();
        grid.unmerge(CellRef::new(0, 0)).unwrap();
        assert_eq!(grid.cell_count(), 6);
        assert_eq!(text_at(&grid, 0, 1), "", "a freed position is a new, empty cell");
        grid.validate().unwrap();
    }

    #[test]
    fn unmerging_a_covered_position_is_rejected_rather_than_redirected() {
        let mut grid = filled(2, 2);
        grid.merge(CellRange::new(0, 0, 2, 2), MergeContent::KeepAnchor).unwrap();
        assert_eq!(
            grid.unmerge(CellRef::new(1, 1)),
            Err(TableError::NotAnAnchor { row: 1, col: 1, anchor_row: 0, anchor_col: 0 })
        );
    }

    #[test]
    fn inserting_a_row_inside_a_vertical_merge_extends_it() {
        let mut grid = filled(3, 2);
        grid.merge(CellRange::new(0, 0, 3, 1), MergeContent::KeepAnchor).unwrap();
        grid.insert_row(1).unwrap();
        assert_eq!(grid.cell(CellRef::new(0, 0)).unwrap().span(), GridSpan::new(4, 1));
        assert_eq!(grid.anchor_at(CellRef::new(1, 0)), Some(CellRef::new(0, 0)));
        assert_eq!(text_at(&grid, 1, 1), "", "the inserted row is empty outside the merge");
        grid.validate().unwrap();
    }

    /// Touching an edge is not straddling it: inserting at a merge's own top row
    /// must put the row above the merge, not inside it.
    #[test]
    fn inserting_at_a_merge_boundary_does_not_extend_it() {
        let mut grid = filled(3, 2);
        grid.merge(CellRange::new(1, 0, 2, 1), MergeContent::KeepAnchor).unwrap();
        grid.insert_row(1).unwrap();
        assert_eq!(grid.cell(CellRef::new(2, 0)).unwrap().span(), GridSpan::new(2, 1));
        assert!(grid.cell(CellRef::new(1, 0)).unwrap().span().is_single());
        grid.validate().unwrap();
    }

    /// The orphan case: the anchor is on the row being deleted, but the cell reaches
    /// past it.
    #[test]
    fn deleting_the_top_row_of_a_merge_re_anchors_it_instead_of_orphaning_the_rest() {
        let mut grid = filled(3, 2);
        grid.merge(CellRange::new(0, 0, 3, 1), MergeContent::KeepAnchor).unwrap();
        let id = grid.cell(CellRef::new(0, 0)).unwrap().id();

        grid.remove_row(0).unwrap();

        let survivor = grid.cell(CellRef::new(0, 0)).unwrap();
        assert_eq!(survivor.span(), GridSpan::new(2, 1));
        assert_eq!(survivor.id(), id, "the same cell moved; it is not a new one");
        assert_eq!(survivor.content().to_plain(), "00");
        grid.validate().unwrap();
    }

    #[test]
    fn deleting_a_row_a_merge_merely_spans_shrinks_it() {
        let mut grid = filled(4, 2);
        grid.merge(CellRange::new(0, 0, 3, 1), MergeContent::KeepAnchor).unwrap();
        grid.remove_row(1).unwrap();
        assert_eq!(grid.cell(CellRef::new(0, 0)).unwrap().span(), GridSpan::new(2, 1));
        grid.validate().unwrap();
    }

    #[test]
    fn a_merge_contained_in_one_row_dies_with_that_row() {
        let mut grid = filled(2, 3);
        grid.merge(CellRange::new(0, 0, 1, 3), MergeContent::KeepAnchor).unwrap();
        grid.remove_row(0).unwrap();
        assert_eq!(grid.rows(), 1);
        assert_eq!(grid.cell_count(), 3);
        grid.validate().unwrap();
    }

    #[test]
    fn inserting_a_column_inside_a_horizontal_merge_extends_it() {
        let mut grid = filled(2, 3);
        grid.merge(CellRange::new(0, 0, 1, 3), MergeContent::KeepAnchor).unwrap();
        grid.insert_column(1).unwrap();
        assert_eq!(grid.columns(), 4);
        assert_eq!(grid.cell(CellRef::new(0, 0)).unwrap().span(), GridSpan::new(1, 4));
        assert_eq!(text_at(&grid, 1, 1), "", "the inserted column is empty below the merge");
        assert_eq!(text_at(&grid, 1, 2), "11", "the old column 1 moved right");
        grid.validate().unwrap();
    }

    #[test]
    fn deleting_the_leftmost_column_of_a_merge_re_anchors_it() {
        let mut grid = filled(2, 3);
        grid.merge(CellRange::new(0, 0, 1, 3), MergeContent::KeepAnchor).unwrap();
        grid.remove_column(0).unwrap();
        assert_eq!(grid.cell(CellRef::new(0, 0)).unwrap().span(), GridSpan::new(1, 2));
        grid.validate().unwrap();
    }

    #[test]
    fn out_of_range_edits_are_rejected_rather_than_clamped() {
        let mut grid = Grid::new(2, 2);
        assert_eq!(grid.remove_row(2), Err(TableError::RowOutOfBounds { index: 2, count: 2 }));
        assert_eq!(grid.insert_row(3), Err(TableError::RowOutOfBounds { index: 3, count: 2 }));
        assert_eq!(
            grid.merge(CellRange::new(0, 0, 1, 0), MergeContent::default()),
            Err(TableError::EmptyRange { row: 0, col: 0, rows: 1, cols: 0 })
        );
        assert!(grid.insert_row(2).is_ok(), "inserting at the end is appending");
    }

    #[test]
    fn a_range_from_two_corners_normalises_whichever_way_it_was_dragged() {
        let up_left = CellRange::from_corners(CellRef::new(3, 4), CellRef::new(1, 2));
        assert_eq!(up_left, CellRange::new(1, 2, 3, 3));
        assert_eq!(up_left, CellRange::from_corners(CellRef::new(1, 2), CellRef::new(3, 4)));
        assert_eq!(CellRange::single(CellRef::new(2, 2)).cells().count(), 1);
    }
}
