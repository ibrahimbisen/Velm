//! Layout: a table plus an available width in, absolute rectangles out.
//!
//! # The pass order, and why it is a pass order
//!
//! 1. **Column intrinsics** — every cell that spans one column contributes its
//!    max-content width plus its horizontal padding.
//! 2. **Base widths** from each column's [`Sizing`].
//! 3. **Span requirements** — a cell spanning several columns widens the auto ones
//!    among them if they cannot hold it.
//! 4. **Fit** — leftover space distributed, or a squeeze applied.
//! 5. **Row heights**, measured at the widths step 4 settled on.
//! 6. **Rectangles**, from two prefix sums.
//! 7. **Borders**, resolved per shared edge and coalesced into runs.
//!
//! Steps 1–4 do not consult a row height and steps 5–7 do not change a width. That
//! is the whole reason [`Sizing::Auto`] means max-content rather than "whatever
//! fits": with a width-dependent auto-fit, step 5 would feed back into step 1 and
//! the pass order would become a fixed-point iteration. See [`sizing`](crate::sizing).
//!
//! # A table is described, not placed
//!
//! [`Table`] holds no position. The origin is an argument to layout, exactly as
//! `vellum-shapes` defines every shape in a unit box and lets the caller map it onto
//! a world rect. The document owns where a widget is; this crate owns how big its
//! parts are, and keeping those separate means moving a table costs nothing and
//! invalidates nothing.
//!
//! # There is no pixel snapping here
//!
//! Boundaries land on fractional world coordinates and stay there. Snapping a 1px
//! hairline to the device pixel grid — which the design language does want — needs
//! the zoom level and the display's scale factor, neither of which is a property of
//! the table. It belongs in `vellum-render`, once, for every hairline in the app.
//!
//! # Cost
//!
//! A full layout is `O(rows × columns)` and measures each cell twice — once for its
//! intrinsic width, once for its height at the width that resulted. A **column
//! resize** re-runs the same passes but, with a warm [`MeasureCache`], reaches the
//! text engine only for the cells in the resized column: intrinsic widths are
//! width-independent and cached on cell identity, and every other column's height
//! entry is keyed on a width that did not change.

use serde::{Deserialize, Serialize};

use crate::geometry::{EPSILON, Point, Rect, Size};
use crate::grid::{CellId, CellRef, GridSpan};
use crate::measure::{Measure, MeasureCache};
use crate::sizing::{
    Fit, Sizing, TrackIntrinsic, base_sizes, distribute_span_requirement, fit_to_width,
};
use crate::style::{BorderSide, CellKind, ResolvedCellStyle, Rgba, VerticalAlign};
use crate::table::Table;

/// One laid-out cell: everything the renderer needs and nothing it has to derive.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CellLayout {
    /// Where the cell is anchored. For a merged cell this is its top-left position,
    /// which is also the coordinate every API in this crate expects back.
    pub anchor: CellRef,
    pub id: CellId,
    pub span: GridSpan,
    /// The cell's box, boundary to boundary. Adjacent cells share their edge
    /// coordinate exactly — there is no gutter, because a border is drawn *on* a
    /// boundary rather than between two of them.
    pub rect: Rect,
    /// [`rect`](Self::rect) less the cell's padding: where text may go.
    pub content_rect: Rect,
    /// The text block itself, positioned inside [`content_rect`](Self::content_rect)
    /// by the cell's vertical alignment. Horizontal alignment is *not* applied here
    /// — it is a property of each wrapped line, which only the text engine knows the
    /// width of, so it travels in [`style`](Self::style) instead.
    ///
    /// Taller than `content_rect` when the text does not fit, in which case it is
    /// top-aligned regardless: centring overflowing text hides its beginning as well
    /// as its end.
    pub text_rect: Rect,
    pub style: ResolvedCellStyle,
    pub kind: CellKind,
    /// The cell contains something unbreakable that is wider than
    /// [`content_rect`](Self::content_rect) — a long word, a URL, a part number.
    ///
    /// Reported rather than fixed. Widening the column would defeat a width the user
    /// set; breaking the word mid-glyph would corrupt a part number. The renderer
    /// clips, and the app can offer to widen.
    pub content_overflows: bool,
}

/// Which way a border segment runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Orientation {
    /// Along a column boundary, top to bottom.
    Vertical,
    /// Along a row boundary, left to right.
    Horizontal,
}

/// A run of border along one boundary.
///
/// Segments are **deduplicated and coalesced**: an interior edge is claimed by the
/// cells on both sides of it and is emitted once, and consecutive positions along a
/// boundary that resolve to the same stroke become a single segment. Both matter for
/// the same reason — two 1px hairlines on one coordinate blend into a smudged line,
/// and the design language's hairline separation is the only thing holding the UI's
/// hierarchy together.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct BorderSegment {
    pub from: Point,
    pub to: Point,
    pub orientation: Orientation,
    /// Which boundary this runs along: `0..=columns` for vertical segments,
    /// `0..=rows` for horizontal ones. The same index
    /// [`HitTarget`](crate::HitTarget) reports for a resize.
    pub boundary: usize,
    pub side: BorderSide,
}

impl BorderSegment {
    pub fn length(&self) -> f64 {
        (self.to.x - self.from.x).abs() + (self.to.y - self.from.y).abs()
    }
}

/// A table laid out at an origin: absolute rectangles, ready to render.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TableLayout {
    pub origin: Point,
    /// The table's outer extent. Under [`Fit::Width`] this equals the requested
    /// width whenever the table had a flexible track to reach it with — compare the
    /// two to find out whether it did.
    pub size: Size,
    pub column_widths: Vec<f64>,
    pub row_heights: Vec<f64>,
    /// Absolute x of every column boundary, `columns + 1` of them. Always
    /// non-decreasing, so a coordinate can be binary-searched into a column.
    pub column_offsets: Vec<f64>,
    /// Absolute y of every row boundary, `rows + 1` of them.
    pub row_offsets: Vec<f64>,
    /// Every visible cell, row-major by anchor. Merged cells appear once.
    pub cells: Vec<CellLayout>,
    pub borders: Vec<BorderSegment>,
    /// The table's own background, behind every cell fill. `None` means the canvas
    /// shows through.
    pub fill: Option<Rgba>,
    pub(crate) columns: usize,
    pub(crate) rows: usize,
    /// Grid position → index into [`cells`](Self::cells) of the cell occupying it.
    /// Row-major, `rows × columns` long. This is what makes "which cell is at
    /// (r, c)" and "which cell is under this point" `O(1)` in the presence of
    /// merges.
    pub(crate) cell_index: Vec<u32>,
}

impl TableLayout {
    pub fn rows(&self) -> usize {
        self.rows
    }

    pub fn columns(&self) -> usize {
        self.columns
    }

    pub fn is_empty(&self) -> bool {
        self.cells.is_empty()
    }

    /// The cell occupying a grid position, resolving through merges.
    pub fn cell_at(&self, at: CellRef) -> Option<&CellLayout> {
        if at.row >= self.rows || at.col >= self.columns {
            return None;
        }
        let index = self.cell_index[at.row * self.columns + at.col];
        self.cells.get(index as usize)
    }

    /// The rect of a whole row, or `None` if there is no such row.
    pub fn row_rect(&self, row: usize) -> Option<Rect> {
        if row >= self.rows {
            return None;
        }
        Some(Rect::from_edges(
            self.origin.x,
            self.row_offsets[row],
            self.origin.x + self.size.width,
            self.row_offsets[row + 1],
        ))
    }

    pub fn column_rect(&self, column: usize) -> Option<Rect> {
        if column >= self.columns {
            return None;
        }
        Some(Rect::from_edges(
            self.column_offsets[column],
            self.origin.y,
            self.column_offsets[column + 1],
            self.origin.y + self.size.height,
        ))
    }

    /// The table's bounding rect — what the spatial index stores.
    pub fn bounds(&self) -> Rect {
        Rect { origin: self.origin, size: self.size }
    }
}

/// Runs the seven passes. See the module docs.
pub(crate) fn build<M: Measure + ?Sized>(
    table: &Table,
    measure: &mut M,
    cache: &mut MeasureCache,
    fit: Fit,
    origin: Point,
) -> TableLayout {
    let rows = table.row_count();
    let columns = table.column_count();

    // Resolving a style walks six cascade levels and clones a font name, so it is
    // done once per cell here rather than at each of the four points below that
    // needs it.
    let mut resolved: Vec<(CellRef, ResolvedCellStyle)> = Vec::with_capacity(table.cell_count());
    for (at, _) in table.grid().anchors() {
        resolved.push((at, table.resolved_style(at)));
    }

    let column_sizings: Vec<Sizing> = table.columns().iter().map(|c| c.sizing).collect();
    let row_sizings: Vec<Sizing> = table.rows().iter().map(|r| r.sizing).collect();

    // 1. Column intrinsics, from the cells that span exactly one column.
    let mut column_intrinsic = vec![TrackIntrinsic::default(); columns];
    for (at, style) in &resolved {
        let cell = table.grid().cell(*at).expect("anchors() yielded it");
        let intrinsic = cache.intrinsic(measure, cell, &style.text);
        if cell.span().cols == 1 {
            let padding = style.padding.horizontal();
            column_intrinsic[at.col].absorb(TrackIntrinsic::new(
                intrinsic.min_content + padding,
                intrinsic.max_content + padding,
            ));
        }
    }

    // 2 and 3. Base widths, then the requirements of cells that span columns.
    let mut column_widths = base_sizes(&column_sizings, &column_intrinsic, fit);
    for (at, style) in &resolved {
        let cell = table.grid().cell(*at).expect("anchors() yielded it");
        let span = cell.span();
        if span.cols > 1 {
            let intrinsic = cache.intrinsic(measure, cell, &style.text);
            distribute_span_requirement(
                &mut column_widths,
                &column_sizings,
                at.col,
                span.cols,
                intrinsic.max_content + style.padding.horizontal(),
            );
        }
    }

    // 4. Fit.
    if let Fit::Width(target) = fit {
        fit_to_width(&mut column_widths, &column_sizings, target);
    }
    let column_offsets = offsets(origin.x, &column_widths);

    // 5. Row heights, at the widths that resulted. A row's `TrackIntrinsic` carries
    // the height its cells need in `max_content`; the same solver serves both axes.
    let mut row_intrinsic = vec![TrackIntrinsic::default(); rows];
    for (at, style) in &resolved {
        let cell = table.grid().cell(*at).expect("anchors() yielded it");
        let span = cell.span();
        if span.rows != 1 {
            continue;
        }
        let width = content_width(&column_widths, at.col, span.cols, style);
        let height = cache.height(measure, cell, &style.text, width) + style.padding.vertical();
        row_intrinsic[at.row].absorb(TrackIntrinsic::new(height, height));
    }
    let mut row_heights = base_sizes(&row_sizings, &row_intrinsic, Fit::Natural);
    for (at, style) in &resolved {
        let cell = table.grid().cell(*at).expect("anchors() yielded it");
        let span = cell.span();
        if span.rows > 1 {
            let width = content_width(&column_widths, at.col, span.cols, style);
            let height = cache.height(measure, cell, &style.text, width) + style.padding.vertical();
            distribute_span_requirement(&mut row_heights, &row_sizings, at.row, span.rows, height);
        }
    }
    let row_offsets = offsets(origin.y, &row_heights);

    // 6. Rectangles.
    let mut cells: Vec<CellLayout> = Vec::with_capacity(resolved.len());
    for (at, style) in &resolved {
        let cell = table.grid().cell(*at).expect("anchors() yielded it");
        let span = cell.span();
        let rect = Rect::from_edges(
            column_offsets[at.col],
            row_offsets[at.row],
            column_offsets[at.col + span.cols],
            row_offsets[at.row + span.rows],
        );
        let content_rect = rect.inset(style.padding);
        // Deliberately the same expression pass 5 used, not `content_rect.width()`:
        // the two agree to within an ulp, and an ulp is enough to miss the cache and
        // silently double the measurement cost of every layout.
        let width = content_width(&column_widths, at.col, span.cols, style);
        let text_height = cache.height(measure, cell, &style.text, width);
        let intrinsic = cache.intrinsic(measure, cell, &style.text);
        cells.push(CellLayout {
            anchor: *at,
            id: cell.id(),
            span,
            rect,
            content_rect,
            text_rect: align_vertically(content_rect, text_height, style.vertical_align),
            kind: table.cell_kind(*at),
            content_overflows: intrinsic.min_content > content_rect.width() + EPSILON,
            style: style.clone(),
        });
    }

    let mut cell_index = vec![0u32; rows * columns];
    for (index, laid_out) in cells.iter().enumerate() {
        for covered in crate::grid::CellRange::of(laid_out.anchor, laid_out.span).cells() {
            cell_index[covered.row * columns + covered.col] = index as u32;
        }
    }

    let size = Size::new(
        column_offsets.last().copied().unwrap_or(origin.x) - origin.x,
        row_offsets.last().copied().unwrap_or(origin.y) - origin.y,
    );

    let mut layout = TableLayout {
        origin,
        size,
        column_widths,
        row_heights,
        column_offsets,
        row_offsets,
        cells,
        borders: Vec::new(),
        fill: table.style().fill,
        columns,
        rows,
        cell_index,
    };
    let borders = build_borders(table, &layout);
    layout.borders = borders;
    layout
}

/// Prefix sums, starting at `start`. `n + 1` long, so the last entry is the total.
fn offsets(start: f64, sizes: &[f64]) -> Vec<f64> {
    let mut out = Vec::with_capacity(sizes.len() + 1);
    let mut running = start;
    out.push(running);
    for size in sizes {
        running += size;
        out.push(running);
    }
    out
}

/// The width text is measured at: the cell's own columns, less its padding.
///
/// Summed from `column_widths` rather than taken as the difference of two entries in
/// `column_offsets`, which would be the obvious thing and is subtly wrong. The two
/// agree to within an ulp, but the *offsets* of every column after a resized one
/// move — so `offsets[j+1] - offsets[j]` changes in its last bit for columns that
/// did not change at all. The height cache is keyed on this width, so that one bit
/// would invalidate the whole table on every drag and quietly turn the `O(rows)`
/// resize back into `O(rows × columns)`. Summing widths is translation-invariant,
/// which is exactly the property the cache needs.
fn content_width(
    column_widths: &[f64],
    col: usize,
    span_cols: usize,
    style: &ResolvedCellStyle,
) -> f64 {
    let end = (col + span_cols).min(column_widths.len());
    let full: f64 = column_widths[col..end].iter().sum();
    (full - style.padding.horizontal()).max(0.0)
}

/// Places a text block of `height` inside `content`.
///
/// Overflowing text is top-aligned whatever the setting says: with nowhere to put
/// the excess, centring it would hide the beginning of the text as well as the end,
/// and the beginning is the part that identifies which cell you are looking at.
fn align_vertically(content: Rect, height: f64, align: VerticalAlign) -> Rect {
    let free = (content.height() - height).max(0.0);
    let top = content.top()
        + match align {
            VerticalAlign::Top => 0.0,
            VerticalAlign::Middle => free * 0.5,
            VerticalAlign::Bottom => free,
        };
    Rect::new(content.left(), top, content.width(), height)
}

/// Walks every boundary position, resolves the stroke on it, and coalesces equal
/// consecutive strokes into runs.
///
/// The two things this is here to get right:
///
/// - **An interior edge belongs to two cells.** Both may state a border; exactly one
///   is drawn, per [`BorderSide::resolve_shared`].
/// - **A merged cell has no interior edges.** A position whose two sides resolve to
///   the same cell is inside a merge, and no line is drawn across it. This is what
///   makes a merged block read as one cell rather than as a cell with its dividers
///   removed by hand.
fn build_borders(table: &Table, layout: &TableLayout) -> Vec<BorderSegment> {
    let (rows, columns) = (layout.rows, layout.columns);
    let mut segments = Vec::new();
    let outer = table.style().outer_border;
    let interior = table.style().grid_border;

    for boundary in 0..=columns {
        let x = layout.column_offsets[boundary];
        let default = if boundary == 0 || boundary == columns { outer } else { interior };
        let mut run: Option<(f64, BorderSide)> = None;
        for row in 0..rows {
            let before = (boundary > 0).then(|| layout.cell_index[row * columns + boundary - 1]);
            let after = (boundary < columns).then(|| layout.cell_index[row * columns + boundary]);
            let side = if before.is_some() && before == after {
                None
            } else {
                BorderSide::resolve_shared(
                    before.and_then(|i| layout.cells[i as usize].style.borders.right.or(default)),
                    after.and_then(|i| layout.cells[i as usize].style.borders.left.or(default)),
                )
                .filter(|s| s.is_visible())
            };
            run = advance_run(
                &mut segments,
                run,
                side,
                layout.row_offsets[row],
                Orientation::Vertical,
                boundary,
                x,
            );
        }
        flush_run(&mut segments, run, layout.row_offsets[rows], Orientation::Vertical, boundary, x);
    }

    for boundary in 0..=rows {
        let y = layout.row_offsets[boundary];
        let default = if boundary == 0 || boundary == rows { outer } else { interior };
        let mut run: Option<(f64, BorderSide)> = None;
        for column in 0..columns {
            let before =
                (boundary > 0).then(|| layout.cell_index[(boundary - 1) * columns + column]);
            let after = (boundary < rows).then(|| layout.cell_index[boundary * columns + column]);
            let side = if before.is_some() && before == after {
                None
            } else {
                BorderSide::resolve_shared(
                    before.and_then(|i| layout.cells[i as usize].style.borders.bottom.or(default)),
                    after.and_then(|i| layout.cells[i as usize].style.borders.top.or(default)),
                )
                .filter(|s| s.is_visible())
            };
            run = advance_run(
                &mut segments,
                run,
                side,
                layout.column_offsets[column],
                Orientation::Horizontal,
                boundary,
                y,
            );
        }
        flush_run(
            &mut segments,
            run,
            layout.column_offsets[columns],
            Orientation::Horizontal,
            boundary,
            y,
        );
    }

    segments
}

/// Extends the open run, or closes it and opens another. `at` is the coordinate
/// along the boundary where the next position starts.
fn advance_run(
    out: &mut Vec<BorderSegment>,
    run: Option<(f64, BorderSide)>,
    side: Option<BorderSide>,
    at: f64,
    orientation: Orientation,
    boundary: usize,
    fixed: f64,
) -> Option<(f64, BorderSide)> {
    match (run, side) {
        (Some((start, open)), Some(side)) if open == side => Some((start, open)),
        (run, side) => {
            flush_run(out, run, at, orientation, boundary, fixed);
            side.map(|side| (at, side))
        }
    }
}

fn flush_run(
    out: &mut Vec<BorderSegment>,
    run: Option<(f64, BorderSide)>,
    end: f64,
    orientation: Orientation,
    boundary: usize,
    fixed: f64,
) {
    let Some((start, side)) = run else { return };
    if end - start <= EPSILON {
        return;
    }
    let (from, to) = match orientation {
        Orientation::Vertical => (Point::new(fixed, start), Point::new(fixed, end)),
        Orientation::Horizontal => (Point::new(start, fixed), Point::new(end, fixed)),
    };
    out.push(BorderSegment { from, to, orientation, boundary, side });
}
