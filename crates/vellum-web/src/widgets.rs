//! The four structured widgets — a table, a chart, a mind map, a kanban board.
//!
//! Every other kind on this board is honestly approximated by a coloured rectangle, which
//! is what [`DrawList::push_scene_item`] draws. These four are not: a table *is* its grid,
//! a chart *is* its bars, a mind map *is* its branches, and a kanban board *is* its
//! columns. Drawn as one flat box each they are indistinguishable from a sticky, and the
//! user has real tables and real kanban boards on real boards — so the browser was showing
//! a blank rectangle where the desktop shows a table. This module is `vellum-app`'s four
//! `draw.rs` arms reduced to what a reader needs, and nothing further.
//!
//! # ⚠ The signature takes a text engine, and it has to
//!
//! **This is the one place this module departs from [`crate::shapes`] and
//! [`crate::strokes`], and it is not a convenience.** All four widgets are laid out by a
//! pure geometry crate that deliberately cannot shape text — `vellum-table`'s own
//! [`MonospaceMeasure`](vellum_table::MonospaceMeasure) says in its doc comment that it
//! *"should never reach the screen"* — so every one of them takes a measurer, and the
//! measurer needs the font stack. A table's column widths and row heights, a kanban card's
//! height, a chart's axis band and a mind map's entire tidy geometry are all functions of
//! measured text.
//!
//! Three routes were considered and two are wrong:
//!
//! - **A second [`TextEngine`] owned here.** Wrong for correctness before it is wrong for
//!   memory: [`crate::text`] draws the words *into the boxes this module measured*, so two
//!   font stacks means text laid out against one set of metrics and drawn with another, and
//!   a cell's words overflow the cell that was sized for them. It is also not constructible
//!   — a browser has no system fonts, so a second engine needs `text.rs`'s bundled faces,
//!   which are private to that module.
//! - **The monospace stand-ins.** Every glyph one ratio wide, so `IIII` and `WWWW` come out
//!   identically wide and every column is visibly the wrong size. That is a layout which
//!   disagrees with the desktop's, drawn confidently.
//! - **Borrow the one engine.** What `vellum-app` does, for the reason its `ShapedMeasure`
//!   records: there is exactly one font stack in the process and a second would answer
//!   questions the first can already answer. `self.widgets` and `self.text` are disjoint
//!   fields of `Viewer`, so `self.widgets.push(…, self.text.engine_mut())` borrows two
//!   different places and the borrow checker is content — the same shape the image arm
//!   already uses with `self.images` and `self.renderer`.
//!
//! # ⚠ World units, not screen pixels
//!
//! Everything below goes to the list in **camera-relative world units**, because the board
//! view's clip transform already carries the zoom. Multiplying by `camera.zoom()` here
//! applies it twice, and the symptom is not subtle-but-wrong: at a fitted 6% every table
//! would be six pixels of grid in the corner of its own box. `lib.rs`'s image arm carries
//! the measurement that produced that lesson.
//!
//! **Dividing by the zoom is a different thing and is correct.** A table's border and a
//! kanban's card outline are *chrome*: they are meant to stay one device pixel however far
//! out the board is, so their thickness is `HAIRLINE / zoom` — computed at push time from
//! the live camera and never baked into a cache, or a hairline would freeze at the zoom the
//! layout happened to be built at.
//!
//! # ⚠ `false` is not a cue to draw the item some other way
//!
//! It means the item was not one of the four. The caller dispatches on the item's *kind*
//! and must not fall back to `push_scene_item` — a flat box behind a table's translucent
//! cell fills is a colour nobody chose, and behind a mind map it is a slab where the desktop
//! draws text on branches. This module draws each widget's own ground where the widget has
//! one, which is why suppressing the fallback loses nothing.
//!
//! # A rotated widget's internals are not rotated
//!
//! Deliberate, and copied rather than improved. `draw.rs` positions every cell, card and
//! node from the **unrotated** box and spins each piece in place, so a turned table draws
//! its grid straight — which is what makes the desktop's press path agree with what is on
//! screen. A browser has no press path, but a board that draws differently in a tab is the
//! defect this whole port exists to avoid.
//!
//! # What is deliberately not here
//!
//! **Words.** [`crate::text`] draws every block in this application and this module draws
//! none. What it does instead is answer [`WidgetLayer::text_slots`] — where each widget's
//! labels go, in world units, with the strings already decoded out of the token. The four
//! kinds answer `None` to [`vellum_doc::ItemKind::text`], so that decoding is the only way
//! the text pass can reach them at all.
//!
//! **A chart's labels.** Not an omission here: `draw.rs`'s chart arm draws no axis labels,
//! no value labels and no legend either, so `text_slots` reports none and the two front ends
//! agree. `vellum-chart` computes all three and nothing consumes them.
//!
//! **Scrolling a kanban board.** `vellum-flow` lays overflow out past the edge on purpose
//! and refuses to shrink its columns; the desktop logs and draws outside the item, and so
//! does this. Clamping here would be a browser board that is not the desktop's board.
//!
//! # Ported constants and helpers, and where they came from
//!
//! `vellum-app` cannot be a dependency of this crate — it is 60,000 lines with a window, a
//! clipboard and SQLite behind it — so the handful of numbers and two mesh builders below
//! are copied, each naming its source. That is the precedent [`crate::shapes`] already sets
//! for its own `HAIRLINE` and its shape decoder. **Every copy is a drift risk**: the durable
//! fix is moving them down into `vellum-project`, which both front ends already depend on.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

use serde::Deserialize;
use vellum_doc::ItemKind;
use vellum_project::project::{Projected, Projection};
use vellum_project::theme::Theme;
use vellum_render::{DrawList, MeshTransform, QuadInstance, Rgba};
use vellum_scene::{Camera, ItemId as SceneId, WorldPoint, WorldRect};
use vellum_shapes::Mesh;
use vellum_text::{LayoutParams, StyledText, TextEngine};

use vellum_chart::{ChartGeometry, ChartSpec, Colour as ChartColour, Rect as ChartRect};
use vellum_flow::{Kanban, KanbanLayout, Rect as FlowRect};
use vellum_mindmap::{
    Color as NodeColour, ConnectorOptions, ConnectorPath, ConnectorShape, Layout as MindMapLayout,
    LayoutKind, LayoutOptions, MindMap, NodeStyle, Size as NodeSize, Vec2,
};
use vellum_table::{
    Fit, Intrinsic, Measure, MeasureCache, Orientation, Point as TablePoint,
    StyledText as TableText, Table, TableLayout, TextStyle as TableTextStyle,
};

// ---------------------------------------------------------------------------------------
// Constants, all copied. Each names the file it was copied from.
// ---------------------------------------------------------------------------------------

/// One device pixel at 100% zoom. `draw.rs`'s `HAIRLINE`, which is private to that module.
///
/// Divided by the zoom wherever it is used, because it is chrome rather than drawing: a
/// table's rules and a kanban card's outline should stay a hairline at any camera.
const HAIRLINE: f64 = 1.0;

/// `draw.rs`'s `STICKY_RADIUS`. The design language pins radii at 4px.
const STICKY_RADIUS: f32 = 4.0;

/// A chart's grid hairline, in world units before the zoom divides it. `draw.rs`.
const GRID_WIDTH: f32 = 1.0;

/// The thinnest a mind map's branch is drawn, in world units. `draw.rs`.
///
/// A fit scale below about 0.3 would otherwise take a 2px branch under half a world unit,
/// and a map shrunk into a small box would lose its lines before it lost its boxes — which
/// reads as broken rather than as small.
const MIN_BRANCH_WIDTH: f32 = 0.75;

/// How finely an arc is flattened: one segment per this many degrees. `vellum-app`'s
/// `chart::ARC_STEP_DEGREES`.
const ARC_STEP_DEGREES: f32 = 6.0;

/// Padding inside a kanban card, around its label. `vellum-app`'s `kanban::CARD_PADDING`.
const CARD_PADDING: f64 = 10.0;

/// Kanban label sizes, in world px. `vellum-app`'s `kanban` module.
const CARD_FONT_SIZE: f64 = 13.0;
const HEADER_FONT_SIZE: f64 = 13.0;
const TITLE_FONT_SIZE: f64 = 15.0;

/// A mind-map node's padding around its measured label. `vellum-app`'s `mindmap` module.
const NODE_PADDING_X: f64 = 14.0;
const NODE_PADDING_Y: f64 = 8.0;
const MIN_NODE_WIDTH: f64 = 56.0;

/// The extent a map with no bounds is treated as having. `vellum-app`'s
/// `mindmap::DEFAULT_SIZE`, and it is a divisor — never zero on either axis.
const DEFAULT_MINDMAP_SIZE: (f64, f64) = (520.0, 260.0);

// ---------------------------------------------------------------------------------------
// What the text pass is handed.
// ---------------------------------------------------------------------------------------

/// One label a structured widget wants drawn, in **world** units.
///
/// Owned throughout — the string is decoded out of a JSON token and the rectangle is
/// derived from a cached layout, so handing out borrows would keep this layer borrowed
/// across `TextLayer::queue`, which needs the engine this layer also borrows.
///
/// ⚠ `slot` is what makes two labels on one item distinguishable. `TextLayer`'s cache key
/// is `(item, generation, size, width)` and nothing else, so a table with two cells of the
/// same width at the same size would draw the *first* cell's words in both — the exact bug
/// `draw.rs`'s `BlockKey::new(id, slot)` exists to prevent. It is stable for as long as the
/// layout is: slots are handed out in the order the widget's own layout lists its pieces.
pub struct WidgetText {
    /// Distinguishes this label from the others on the same item. See above.
    pub slot: u16,
    /// The box the words go in, in absolute world coordinates.
    pub rect: WorldRect,
    /// Already decoded out of the widget's token, with bold folded onto its spans.
    pub text: StyledText,
    /// Explicit, never auto-fitted. A widget's label is set at the size its layout was
    /// measured against; auto-fitting it would make a one-word card enormous and would
    /// disagree with the box the card was sized to.
    pub font_size: f32,
    pub color: Rgba,
    pub anchor: crate::layout::Anchor,
}

// ---------------------------------------------------------------------------------------
// The per-frame layer.
// ---------------------------------------------------------------------------------------

/// A table's laid-out grid, and what it was laid out against.
///
/// Keyed on the generation **and** the box, exactly as `draw.rs`'s `CachedTable` is: a
/// resize changes every column, and holding the size makes that a property of this cache
/// rather than a promise about `Projection::moved`.
///
/// The table itself is held beside its layout because a cell rectangle knows its *anchor*
/// and not its words — the text is reached back through `Table::cell`.
struct CachedTable {
    generation: u64,
    size: (f64, f64),
    layout: TableLayout,
    table: Table,
}

/// A chart's geometry, and what it was built against.
///
/// ⚠ **`draw.rs` rebuilds a chart's geometry every frame and this caches it.** A deliberate
/// deviation and the only one in this file: the inputs are the token and the box, so equal
/// inputs give equal output, and a browser tab is very often on a battery. Nothing here
/// depends on the camera, which is what makes it safe.
struct CachedChart {
    generation: u64,
    size: (f64, f64),
    geometry: ChartGeometry,
}

/// A mind map's laid-out tree.
///
/// Keyed on the generation alone, unlike the table's: a tidy tree's extent comes from the
/// tree and its shaped labels, never from the item's box, so a resize cannot change it.
/// What a resize changes is the fit scale, and that is derived from the box at the point of
/// drawing rather than baked in here.
struct CachedMindMap {
    generation: u64,
    layout: MindMapLayout,
    map: MindMap,
    connectors: Vec<ConnectorPath>,
    /// The map's own extent, which is the divisor in [`fit_scale`]. Never zero either way.
    natural: (f64, f64),
}

/// A kanban board's laid-out columns, and the measured board they were laid out from.
///
/// Both, and they must not be allowed to disagree: `KanbanLayout` carries rectangles and
/// ids, and every label the text pass needs lives on the board those rectangles came from.
struct CachedKanban {
    generation: u64,
    size: (f64, f64),
    layout: KanbanLayout,
    board: Kanban,
}

/// The per-frame drawing of the four structured widgets, plus what survives between frames.
#[derive(Default)]
pub struct WidgetLayer {
    tables: HashMap<SceneId, CachedTable>,
    charts: HashMap<SceneId, CachedChart>,
    mindmaps: HashMap<SceneId, CachedMindMap>,
    kanbans: HashMap<SceneId, CachedKanban>,
    /// Tokens this build could not read, against the generation they were tried at.
    ///
    /// ⚠ **Without this, a broken token is re-parsed and re-logged sixty times a second for
    /// as long as it is on screen.** Every cache above is a cache of *success*, so a failure
    /// falls straight through it and repeats — which costs a JSON parse per frame and, worse,
    /// buries every other line in the console under the same warning. A `log::warn!` on a
    /// per-frame path has to be a *changed*-state warning or it is not a warning at all.
    ///
    /// Keyed on the generation alone and not on the box: a token that does not parse does not
    /// parse at any size, and an edit is what could make it parse.
    unreadable: HashMap<SceneId, u64>,
}

impl WidgetLayer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Draw one item's structure, if it is one of the four. Returns whether anything was
    /// pushed.
    ///
    /// The list must be in the **board** view: every rectangle below is camera-relative and
    /// in world units, so a screen-view list would place each widget at the board's absolute
    /// coordinates in physical pixels.
    ///
    /// ⚠ **Push order is paint order**, whatever the items' z. That is why each arm below
    /// pushes its ground before its cells, its cells before its rules, and its branches
    /// before its nodes — and why a widget cannot be split into two passes over the same
    /// list.
    ///
    /// See the module docs for `engine`, and for why `false` must not be answered with
    /// `push_scene_item`.
    pub fn push(
        &mut self,
        list: &mut DrawList,
        camera: &Camera,
        id: SceneId,
        projection: &Projection,
        theme: &Theme,
        engine: &mut TextEngine,
    ) -> bool {
        let Some(projected) = projection.get(id) else { return false };
        let box_size = projected.item.placement.scaled_size();

        match &projected.item.kind {
            ItemKind::Table { model } => {
                if !self.ensure_table(id, model, projected.generation, box_size, engine) {
                    return placeholder(list, camera, projected, theme);
                }
                let Some(cached) = self.tables.get(&id) else { return false };
                draw_table(list, camera, projected, theme, &cached.layout);
                true
            }
            ItemKind::Chart { spec } => {
                if !self.ensure_chart(id, spec, projected.generation, box_size, engine) {
                    return placeholder(list, camera, projected, theme);
                }
                let Some(cached) = self.charts.get(&id) else { return false };
                draw_chart(list, camera, projected, &cached.geometry);
                true
            }
            ItemKind::MindMap { model } => {
                if !self.ensure_mindmap(id, model, projected.generation, engine) {
                    return placeholder(list, camera, projected, theme);
                }
                let Some(cached) = self.mindmaps.get(&id) else { return false };
                draw_mindmap(list, camera, projected, cached);
                true
            }
            ItemKind::Kanban { board } => {
                if !self.ensure_kanban(id, board, projected.generation, box_size, engine) {
                    return placeholder(list, camera, projected, theme);
                }
                let Some(cached) = self.kanbans.get(&id) else { return false };
                draw_kanban(list, camera, projected, theme, &cached.layout);
                true
            }
            _ => false,
        }
    }

    /// Where this item's labels go, if it is one of the four and its layout is in hand.
    ///
    /// Reads the caches [`Self::push`] fills and never builds one, which is why it needs no
    /// engine — and why **the caller must push before it asks**. That is not a burden: the
    /// frame loop already draws every visible item before it queues any text, and it is what
    /// guarantees the rectangles the words go in are the rectangles the widget was just drawn
    /// with. An item that has never been pushed, and one whose token this build cannot read,
    /// both answer with nothing.
    ///
    /// Rectangles are **absolute world coordinates**, taken from the item's unrotated
    /// top-left — the same convention the drawing uses, and for the same reason.
    pub fn text_slots(&self, id: SceneId, projection: &Projection, theme: &Theme) -> Vec<WidgetText> {
        let Some(projected) = projection.get(id) else { return Vec::new() };
        let (origin, size) = projected.rect();
        match &projected.item.kind {
            ItemKind::Table { .. } => {
                self.tables.get(&id).map_or_else(Vec::new, |c| table_slots(c, origin, theme))
            }
            ItemKind::MindMap { .. } => self
                .mindmaps
                .get(&id)
                .map_or_else(Vec::new, |c| mindmap_slots(c, origin, size)),
            ItemKind::Kanban { .. } => {
                self.kanbans.get(&id).map_or_else(Vec::new, |c| kanban_slots(c, origin, theme))
            }
            // A chart's labels are computed by `vellum-chart` and drawn by neither front
            // end. See the module docs.
            _ => Vec::new(),
        }
    }

    /// Drop the layout of anything no longer on screen.
    ///
    /// Without this the caches are a leak with a nice name: a table holds its whole laid-out
    /// grid *and* a clone of the table it came from, a kanban a clone of its measured board,
    /// and wasm linear memory never returns anything to the OS — so panning across a board
    /// of widgets makes the peak permanent. The same argument [`crate::text`],
    /// [`crate::strokes`] and [`crate::shapes`] all make.
    ///
    /// ⚠ **No size guard.** The count of cached layouts and the count of visible items
    /// measure different populations — one entry per *widget* against one per visible item of
    /// every kind — so a `len() <= on_screen.len()` early-out fires on essentially every
    /// frame and releases nothing. That guard was in `strokes.rs`, was measured firing at 219
    /// against 596 on the reference board, and was removed; it is not re-introduced here.
    pub fn retain_visible(&mut self, on_screen: &[SceneId]) {
        if self.tables.is_empty()
            && self.charts.is_empty()
            && self.mindmaps.is_empty()
            && self.kanbans.is_empty()
            && self.unreadable.is_empty()
        {
            return;
        }
        let keep: HashSet<SceneId> = on_screen.iter().copied().collect();
        self.tables.retain(|id, _| keep.contains(id));
        self.charts.retain(|id, _| keep.contains(id));
        self.mindmaps.retain(|id, _| keep.contains(id));
        self.kanbans.retain(|id, _| keep.contains(id));
        // Pruned with the rest, and it is in the early-out above for the same reason: a board
        // of broken tokens would otherwise grow one entry per item ever seen and never
        // release one, which is the leak this method exists to close wearing a different hat.
        self.unreadable.retain(|id, _| keep.contains(id));
    }

    /// The widget a token names, or `None` — **once** per generation, however many frames it
    /// is on screen for.
    ///
    /// **A token this build does not understand is a degradation, not an error and not a
    /// missing item** — the rule the whole document layer follows, and the caller's answer to
    /// the `None` is [`placeholder`] rather than an empty board.
    ///
    /// The remembering is the point rather than an optimisation; see `unreadable`. The
    /// `remove` on the success path matters as much: an edit that repairs a token has to be
    /// able to clear the mark, or a board fixed in one tab stays broken in the other.
    fn readable<T: serde::de::DeserializeOwned>(
        &mut self,
        id: SceneId,
        token: &str,
        generation: u64,
        kind: &str,
    ) -> Option<T> {
        if self.unreadable.get(&id) == Some(&generation) {
            return None;
        }
        let parsed = if token.is_empty() {
            log::warn!("an empty {kind} token; drawing a placeholder");
            None
        } else {
            match serde_json::from_str(token) {
                Ok(value) => Some(value),
                Err(error) => {
                    // Logged, not swallowed. "Nothing readable here" and "this feature is not
                    // wired up" look identical on screen, and the one line that tells them
                    // apart costs a console write on a path that does not normally run.
                    // `shapes.rs` and `strokes.rs` both warn here for the same reason.
                    log::warn!("unreadable {kind} ({error}); drawing a placeholder");
                    None
                }
            }
        };
        if parsed.is_none() {
            self.unreadable.insert(id, generation);
            // ⚠ **Drop what this item had cached, or the placeholder gets the old widget's
            // words drawn on top of it.** The caches are keyed on the generation, so an edit
            // that breaks a token leaves the *previous* generation's layout sitting in the
            // map — `push` never reads it again, but [`Self::text_slots`] would, and it does
            // not know a placeholder was drawn instead. A grid nobody can see with last
            // edit's labels floating over it is worse than either half alone.
            //
            // All four maps rather than the one this call is for: an item's kind cannot
            // change, so at most one can hold it, and three misses on a `HashMap` is the
            // version that cannot be got wrong by adding a fifth widget later.
            self.tables.remove(&id);
            self.charts.remove(&id);
            self.mindmaps.remove(&id);
            self.kanbans.remove(&id);
        } else {
            self.unreadable.remove(&id);
        }
        parsed
    }

    /// Lay this table out if the cache has not got it. `false` means the token is unreadable.
    fn ensure_table(
        &mut self,
        id: SceneId,
        token: &str,
        generation: u64,
        size: (f64, f64),
        engine: &mut TextEngine,
    ) -> bool {
        if self
            .tables
            .get(&id)
            .is_some_and(|c| c.generation == generation && c.size == size)
        {
            return true;
        }
        let Some(table) = self.readable::<Table>(id, token, generation, "table") else {
            return false;
        };
        // `Fit::Width` rather than the table's natural size, and the origin is the item's
        // own top-left: an item on a board has a box the user dragged, and every rectangle
        // that comes back is an offset inside it.
        let mut measure = ShapedMeasure::new(engine);
        let mut cache = MeasureCache::default();
        let layout =
            table.layout(&mut measure, &mut cache, Fit::Width(size.0), TablePoint::new(0.0, 0.0));
        self.tables.insert(id, CachedTable { generation, size, layout, table });
        true
    }

    fn ensure_chart(
        &mut self,
        id: SceneId,
        token: &str,
        generation: u64,
        size: (f64, f64),
        engine: &mut TextEngine,
    ) -> bool {
        if self
            .charts
            .get(&id)
            .is_some_and(|c| c.generation == generation && c.size == size)
        {
            return true;
        }
        let Some(spec) = self.readable::<ChartSpec>(id, token, generation, "chart") else {
            return false;
        };
        let frame = ChartRect::new(0.0, 0.0, size.0 as f32, size.1 as f32);
        let metrics = ShapedMetrics::new(engine);
        let geometry = vellum_chart::build(&spec, frame, &metrics);
        self.charts.insert(id, CachedChart { generation, size, geometry });
        true
    }

    fn ensure_mindmap(
        &mut self,
        id: SceneId,
        token: &str,
        generation: u64,
        engine: &mut TextEngine,
    ) -> bool {
        if self.mindmaps.get(&id).is_some_and(|c| c.generation == generation) {
            return true;
        }
        let Some(model) = self.readable::<MindMapModel>(id, token, generation, "mind map") else {
            return false;
        };
        let map = measure_nodes(&model.map, engine);
        let options = LayoutOptions { kind: model.kind, ..LayoutOptions::default() };
        let mut layout = map.layout(&options);
        // Without this a tree layout starts at the root's centre and half the map has
        // negative coordinates. Translating it makes a node rectangle an offset from the
        // item's top-left, which is the convention the table's cells already use.
        if let Some(bounds) = layout.bounds() {
            layout.translate(Vec2::new(-bounds.min.x, -bounds.min.y));
        }
        let connectors = layout.connectors(&ConnectorOptions {
            shape: model.connectors,
            ..ConnectorOptions::default()
        });
        let natural = layout
            .bounds()
            .map_or(DEFAULT_MINDMAP_SIZE, |b| (b.width().max(1.0), b.height().max(1.0)));
        self.mindmaps
            .insert(id, CachedMindMap { generation, layout, map, connectors, natural });
        true
    }

    fn ensure_kanban(
        &mut self,
        id: SceneId,
        token: &str,
        generation: u64,
        size: (f64, f64),
        engine: &mut TextEngine,
    ) -> bool {
        if self
            .kanbans
            .get(&id)
            .is_some_and(|c| c.generation == generation && c.size == size)
        {
            return true;
        }
        let Some(board) = self.readable::<Kanban>(id, token, generation, "kanban board") else {
            return false;
        };
        let measured = measure_cards(&board, engine, size);
        let layout = measured.layout(FlowRect::new(0.0, 0.0, size.0, size.1));
        if layout.overflows() {
            // `vellum-flow` lays overflow out past the edge on purpose and nothing here
            // scrolls, so this draws outside its own item. Logged rather than clamped:
            // shrinking the columns is the behaviour that crate explicitly refuses, and
            // resizing the item is the remedy. `draw.rs` says the same at the same point.
            log::debug!("a kanban board overflows its box; nothing here scrolls");
        }
        self.kanbans
            .insert(id, CachedKanban { generation, size, layout, board: measured });
        true
    }
}

// ---------------------------------------------------------------------------------------
// Drawing. One function per kind, each a reduction of the matching `draw.rs` arm.
// ---------------------------------------------------------------------------------------

/// A table: its ground, then its cell fills, then its rules.
///
/// That order is the drawing. Borders are drawn **on** a boundary rather than between two
/// cells — `vellum-table` has already coalesced them, so an interior edge is one segment
/// claimed by one side rather than two overlapping hairlines.
fn draw_table(
    list: &mut DrawList,
    camera: &Camera,
    projected: &Projected,
    theme: &Theme,
    laid: &TableLayout,
) {
    let (position, size, rotation, opacity) = frame_of(camera, projected);
    let at = |x: f64, y: f64| [position[0] + x as f32, position[1] + y as f32];

    // The table's own ground, behind every cell. `None` means the canvas shows through,
    // on the desktop as here — this is not a missing background to invent one for.
    if let Some(fill) = laid.fill {
        list.push_quad(
            QuadInstance::solid(position, size, table_colour(fill))
                .with_rotation(rotation)
                .with_opacity(opacity),
        );
    }
    for cell in &laid.cells {
        let Some(fill) = cell.style.fill else { continue };
        list.push_quad(
            QuadInstance::solid(
                at(cell.rect.origin.x, cell.rect.origin.y),
                [cell.rect.size.width as f32, cell.rect.size.height as f32],
                table_colour(fill),
            )
            .with_rotation(rotation)
            .with_opacity(opacity),
        );
    }
    for border in &laid.borders {
        let (x, y) = (border.from.x.min(border.to.x), border.from.y.min(border.to.y));
        // ⚠ Divided by the zoom, not multiplied: a rule is chrome and stays one device
        // pixel however far out the camera is. Computed here rather than in the cache so it
        // follows a zoom without invalidating a layout.
        let thickness = border.side.width.max(HAIRLINE / camera.zoom());
        let (w, h) = match border.orientation {
            Orientation::Vertical => (thickness, border.length()),
            Orientation::Horizontal => (border.length(), thickness),
        };
        let colour = border.side.color.map_or(theme.border, table_colour);
        list.push_quad(
            QuadInstance::solid(at(x, y), [w as f32, h as f32], colour)
                .with_rotation(rotation)
                .with_opacity(opacity),
        );
    }
}

/// A chart: its ground, its gridlines and baseline, then its marks.
///
/// The rules go **under** the marks: a bar sitting on the baseline should cover it, not be
/// cut by it. Every gap and ring between marks is the ground showing through rather than
/// paint, which is why the ground has to be first.
fn draw_chart(list: &mut DrawList, camera: &Camera, projected: &Projected, geometry: &ChartGeometry) {
    let (position, size, rotation, opacity) = frame_of(camera, projected);
    let at = |x: f32, y: f32| [position[0] + x, position[1] + y];
    let tint = |c: ChartColour| {
        Rgba::from_rgb8(c.r, c.g, c.b).with_alpha(f32::from(c.a) / 255.0 * opacity)
    };

    list.push_quad(
        QuadInstance::solid(position, size, tint(geometry.surface)).with_rotation(rotation),
    );

    for axis in &geometry.axes {
        for tick in &axis.ticks {
            if let Some(grid) = tick.gridline {
                push_chart_segment(list, &at, grid, GRID_WIDTH, tint(axis.colour));
            }
        }
    }
    if let Some(baseline) = geometry.baseline {
        let colour = tint(geometry.baseline_colour);
        push_chart_segment(list, &at, baseline, GRID_WIDTH * 1.5, colour);
    }

    for mark in &geometry.marks {
        match mark {
            vellum_chart::Mark::Bar(bar) => {
                list.push_quad(
                    QuadInstance::solid(
                        at(bar.rect.x, bar.rect.y),
                        [bar.rect.width, bar.rect.height],
                        tint(bar.colour),
                    )
                    .with_corner_radius(bar.radius)
                    .with_rotation(rotation),
                );
            }
            vellum_chart::Mark::Dot(dot) => {
                let d = dot.radius * 2.0;
                list.push_quad(
                    QuadInstance::solid(
                        at(dot.centre.x - dot.radius, dot.centre.y - dot.radius),
                        [d, d],
                        tint(dot.colour),
                    )
                    // A full-radius corner on a square is a circle, which saves a second
                    // pipeline for a scatter point.
                    .with_corner_radius(dot.radius)
                    .with_rotation(rotation),
                );
            }
            vellum_chart::Mark::Line(line) => push_local_mesh(
                list,
                position,
                &ribbon(&polyline_points(&line.path), line.width),
                tint(line.colour),
                rotation,
            ),
            vellum_chart::Mark::Area(area) => {
                push_local_mesh(list, position, &ring_mesh(&area.outline), tint(area.fill), rotation);
            }
            vellum_chart::Mark::Slice(slice) => {
                push_local_mesh(list, position, &slice_mesh(&slice.arc), tint(slice.colour), rotation);
            }
        }
    }
}

/// A mind map: its branches, then its nodes.
///
/// **No ground.** `draw.rs` pushes none either, and it is the right answer rather than an
/// omission: an unstyled mind map is text on branches, which is what a mind map is, and a
/// slab behind it would be a surface nobody chose.
///
/// Branches under the nodes: a link runs to a node's border, and a node with a fill should
/// cover the last hairline of it rather than be cut by it.
fn draw_mindmap(list: &mut DrawList, camera: &Camera, projected: &Projected, cached: &CachedMindMap) {
    let (position, size, rotation, opacity) = frame_of(camera, projected);
    let scale = fit_scale(cached.natural, (f64::from(size[0]), f64::from(size[1])));
    let at = |x: f64, y: f64| [position[0] + x as f32, position[1] + y as f32];
    let tint =
        |c: NodeColour| Rgba::from_rgb8(c.r, c.g, c.b).with_alpha(f32::from(c.a) / 255.0 * opacity);

    for path in &cached.connectors {
        // A branch's colour and width live on the **child**, which is what
        // `vellum-mindmap`'s connector module says: a link belongs to the branch it feeds,
        // not to the parent it leaves.
        let Some(node) = cached.map.get(path.child) else { continue };
        let style = node.style;
        if !style.connector.is_visible() {
            continue;
        }
        let points: Vec<[f32; 2]> = path
            .points
            .iter()
            .map(|p| [(p.x * scale) as f32, (p.y * scale) as f32])
            .collect();
        let mesh = ribbon(&points, ((style.connector_width * scale) as f32).max(MIN_BRANCH_WIDTH));
        push_local_mesh(list, position, &mesh, tint(style.connector), rotation);
    }

    for placement in cached.layout.placements() {
        let Some(node) = cached.map.get(placement.node) else { continue };
        let style = node.style;
        // An unstyled node is text on a branch rather than a box — `NodeStyle`'s default is
        // transparent both ways and its own test pins that — so a node with neither fill nor
        // border draws nothing and costs no quad.
        if !style.fill.is_visible() && !style.border.is_visible() {
            continue;
        }
        let rect = placement.rect;
        let quad = QuadInstance::solid(
            at(rect.min.x * scale, rect.min.y * scale),
            [(rect.width() * scale) as f32, (rect.height() * scale) as f32],
            tint(style.fill),
        )
        .with_corner_radius((style.corner_radius * scale) as f32)
        .with_rotation(rotation)
        .with_opacity(opacity);
        let quad = if style.border.is_visible() {
            quad.with_border(tint(style.border), (style.border_width * scale) as f32)
        } else {
            quad
        };
        list.push_quad(quad);
    }
}

/// A kanban board: its own ground, then a column's, then its cards.
///
/// Three surfaces a step apart, which is how the design language asks for hierarchy — a
/// luminance step and a hairline, not a shadow.
fn draw_kanban(
    list: &mut DrawList,
    camera: &Camera,
    projected: &Projected,
    theme: &Theme,
    laid: &KanbanLayout,
) {
    let (position, size, rotation, opacity) = frame_of(camera, projected);
    let at = |r: FlowRect| {
        (
            [position[0] + r.left() as f32, position[1] + r.top() as f32],
            [r.width() as f32, r.height() as f32],
        )
    };
    let hairline = (HAIRLINE / camera.zoom()) as f32;

    list.push_quad(
        QuadInstance::solid(position, size, theme.frame_fill)
            .with_corner_radius(STICKY_RADIUS)
            .with_border(theme.border, hairline)
            .with_rotation(rotation)
            .with_opacity(opacity),
    );
    for column in &laid.columns {
        let (origin, extent) = at(column.rect);
        list.push_quad(
            QuadInstance::solid(origin, extent, theme.canvas)
                .with_corner_radius(STICKY_RADIUS)
                .with_rotation(rotation)
                .with_opacity(opacity),
        );
        // A breached column is marked on its header rather than by tinting the whole
        // column: the accent is meant to be scarce, and the header is where the count that
        // breached is written. A breach is state, not colour — `vellum-flow` says so
        // explicitly and holds no hex literal — so the decision of what one *looks* like is
        // made here, against the theme.
        let over = column.wip.limit.is_some_and(|limit| column.wip.count > limit);
        let (origin, extent) = at(column.header);
        let tint = if over { theme.accent } else { theme.border };
        list.push_quad(
            QuadInstance::solid(origin, [extent[0], hairline.max(1.0)], tint)
                .with_rotation(rotation)
                .with_opacity(opacity),
        );
    }
    // ⚠ **Every card after every column**, rather than a column's cards inside its own
    // iteration. Push order is paint order, so the two orders are only equivalent while
    // nothing overlaps — and `vellum-flow` lays overflow out *past the edge* on purpose, so
    // "nothing overlaps" is exactly the assumption a kanban board is entitled to break.
    // `draw.rs` flattens the cards across all columns for this reason and this matches it,
    // which costs one extra walk of a list that is already in cache.
    for column in &laid.columns {
        for card in &column.cards {
            let (origin, extent) = at(card.rect);
            list.push_quad(
                QuadInstance::solid(origin, extent, theme.frame_fill)
                    .with_corner_radius(STICKY_RADIUS)
                    .with_border(theme.border, hairline)
                    .with_rotation(rotation)
                    .with_opacity(opacity),
            );
        }
    }
}

/// The box drawn for a widget whose token this build cannot read.
///
/// ⚠ **This deliberately differs from the desktop's decoders, and the difference is
/// deliberate in one direction only.** `vellum-app`'s `decode` answers with a *default*
/// widget — a three-column Sprint board with six named cards, a chart of invented numbers, a
/// mind map of invented branches — so that the item stays editable. A viewer has nothing to
/// edit, and drawing invented card labels on somebody's board is a lie about its contents.
///
/// It is not nothing, either: `push_scene_item` is suppressed for these four kinds, so
/// drawing nothing would make the item vanish, and an item that exists on the desktop and is
/// invisible in a tab reads as data loss. A surface with a hairline says *there is something
/// here this build cannot draw*, which is the honest answer and is what the console line
/// beside it names.
fn placeholder(list: &mut DrawList, camera: &Camera, projected: &Projected, theme: &Theme) -> bool {
    let (position, size, rotation, opacity) = frame_of(camera, projected);
    list.push_quad(
        QuadInstance::solid(position, size, theme.surface)
            .with_corner_radius(STICKY_RADIUS)
            .with_border(theme.border, (HAIRLINE / camera.zoom()) as f32)
            .with_rotation(rotation)
            .with_opacity(opacity),
    );
    true
}

/// One axis rule or baseline, as a quad.
///
/// Chart segments are always axis-aligned — a grid line, a zero rule, an axis spine — so a
/// quad is exact rather than an approximation, and cheaper than a mesh.
///
/// ⚠ No rotation, matching `draw.rs`. A rotated chart therefore draws its gridlines
/// straight while its bars turn. That is the desktop's behaviour today and is copied rather
/// than corrected; correcting it here alone would make a browser board differ from the Mac's.
fn push_chart_segment(
    list: &mut DrawList,
    at: &impl Fn(f32, f32) -> [f32; 2],
    segment: vellum_chart::Segment,
    width: f32,
    colour: Rgba,
) {
    let (from, to) = (segment.from, segment.to);
    let (x, y) = (from.x.min(to.x), from.y.min(to.y));
    let (w, h) = ((to.x - from.x).abs().max(width), (to.y - from.y).abs().max(width));
    list.push_quad(QuadInstance::solid(at(x, y), [w, h], colour));
}

/// One tessellated mark, in its item's own space.
///
/// The vertices start at the item's top-left — which is how both a chart's marks and a mind
/// map's branches are laid out — so the transform is that corner and the mesh needs no
/// per-vertex offset. Copied from `draw.rs`'s `push_local_mesh`.
fn push_local_mesh(
    list: &mut DrawList,
    position: [f32; 2],
    mesh: &Mesh,
    colour: Rgba,
    rotation: f32,
) {
    if mesh.indices.is_empty() {
        return;
    }
    let transform = list
        .meshes_mut()
        .push_transform(MeshTransform::scale_rotate_at(1.0, rotation, position));
    let start = list.meshes().indices().len() as u32;
    list.meshes_mut().push_shape_fill(mesh, colour, transform);
    let end = list.meshes().indices().len() as u32;
    if end == start {
        // A transform was pushed and nothing referenced it. Pushing an empty range would
        // open a mesh batch for zero triangles and split the batch either side of it —
        // `shapes.rs` records the same guard for the same reason.
        return;
    }
    list.push_meshes(start..end);
}

/// The item's camera-relative corner, its world extent, its rotation and its opacity.
///
/// ⚠ **`rect()`, never `bounds`.** `bounds` is the axis-aligned box a rotated item
/// *encloses*, which is what the R-tree culls and hit-tests against and is strictly larger
/// than the item for anything turned off the axis. Every widget below lays itself out inside
/// the unrotated box and spins the pieces in place, so feeding it the enclosing box would
/// draw a rotated table too big and centred correctly — which looks like a scale bug.
fn frame_of(camera: &Camera, projected: &Projected) -> ([f32; 2], [f32; 2], f32, f32) {
    let (origin, (width, height)) = projected.rect();
    (
        camera.to_camera_relative(origin),
        [width as f32, height as f32],
        projected.rotation(),
        projected.opacity(),
    )
}

// ---------------------------------------------------------------------------------------
// Text slots.
// ---------------------------------------------------------------------------------------

/// One slot per non-empty cell, in the order `vellum-table` lists them.
///
/// The rectangle is `content_rect` — the cell less its padding, which is where text may go —
/// rather than `rect`, so a cell's words do not sit on the rule beside them. `draw.rs`'s
/// `table_cell_block` reads the same field.
///
/// ⚠ **Horizontal alignment inside a cell is dropped.** `vellum-table` carries Left, Center
/// and Right per cell; `TextLayer::queue` takes an [`Anchor`](crate::layout::Anchor) with
/// two values, and its `Centred` centres *both* axes — which for a top-aligned cell would
/// move the words down as well as across. Left is the crate's own default and is right for
/// the common case; a centred cell is set left in a tab and is the honest gap.
fn table_slots(cached: &CachedTable, origin: WorldPoint, theme: &Theme) -> Vec<WidgetText> {
    let mut out = Vec::new();
    for (index, cell) in cached.layout.cells.iter().enumerate() {
        // The words live on the table and are reached by the cell's anchor; the layout
        // carries where they go and not what they say.
        let Some(content) = cached.table.cell(cell.anchor) else { continue };
        let text = content.content();
        if text.to_plain().trim().is_empty() {
            continue;
        }
        let colour = cell
            .style
            .text
            .color
            .map_or(theme.text, |c| Rgba::from_rgb8(c.r, c.g, c.b));
        out.push(WidgetText {
            slot: slot_of(index),
            rect: world_rect(
                origin,
                cell.content_rect.origin.x,
                cell.content_rect.origin.y,
                cell.content_rect.size.width,
                cell.content_rect.size.height,
            ),
            text: table_text(text, &cell.style.text),
            font_size: cell.style.text.font_size as f32,
            color: colour,
            anchor: crate::layout::Anchor::TopLeft,
        });
    }
    out
}

/// One slot per visible node, centred in its own box.
///
/// [`Anchor::Centred`](crate::layout::Anchor) is exactly right here and nowhere else in this
/// file: a node's box was *measured from* its label plus padding, so centring the words in
/// it puts them where the measurement said they would be.
fn mindmap_slots(cached: &CachedMindMap, origin: WorldPoint, size: (f64, f64)) -> Vec<WidgetText> {
    let scale = fit_scale(cached.natural, size);
    let mut out = Vec::new();
    for (index, placement) in cached.layout.placements().iter().enumerate() {
        let Some(node) = cached.map.get(placement.node) else { continue };
        if node.text.trim().is_empty() {
            continue;
        }
        let rect = placement.rect;
        out.push(WidgetText {
            slot: slot_of(index),
            rect: world_rect(
                origin,
                rect.min.x * scale,
                rect.min.y * scale,
                rect.width() * scale,
                rect.height() * scale,
            ),
            text: node_label(&node.text, &node.style),
            font_size: (node.style.font_size * scale) as f32,
            color: {
                let c = node.style.text;
                Rgba::from_rgb8(c.r, c.g, c.b).with_alpha(f32::from(c.a) / 255.0)
            },
            anchor: crate::layout::Anchor::Centred,
        });
    }
    out
}

/// The board's title, then each column's header, then that column's cards.
///
/// A reduction of `draw.rs`'s `kanban_runs`, which is `pub(crate)` there and cannot be
/// imported. The order is the same and matters for the same reason: a slot is an index into
/// this flattened list, so it has to be produced the same way every frame.
///
/// The header carries the count — and the limit when there is one, which is what makes a WIP
/// limit visible rather than merely enforced. `vellum-flow` computes the numbers; the wording
/// is the only thing decided here, and it is decided identically on both front ends.
fn kanban_slots(cached: &CachedKanban, origin: WorldPoint, theme: &Theme) -> Vec<WidgetText> {
    let pad = CARD_PADDING;
    let laid = &cached.layout;
    let board = &cached.board;
    let mut out = Vec::new();
    let mut index = 0usize;
    let push = |out: &mut Vec<WidgetText>,
                index: &mut usize,
                rect: FlowRect,
                words: String,
                font_size: f64,
                colour: Rgba| {
        // ⚠ The slot is taken and the counter advanced **before** the empty test, and every
        // caller below hands over an empty string rather than skipping. A slot is a text
        // cache key, so it has to name the same piece of the board every frame — and this is
        // the one of the three lists whose index is a running counter rather than an
        // `enumerate`, so a label that quietly did not consume one would renumber every label
        // after it the moment a card was given a word. That draws one card's text on another,
        // which is exactly what `draw.rs`'s `kanban_runs` and its stale-slot note are about.
        let slot = slot_of(*index);
        *index += 1;
        if words.trim().is_empty() {
            return;
        }
        out.push(WidgetText {
            slot,
            rect: world_rect(origin, rect.left(), rect.top(), rect.width(), rect.height()),
            text: StyledText::plain(&words),
            font_size: font_size as f32,
            color: colour,
            anchor: crate::layout::Anchor::TopLeft,
        });
    };

    push(
        &mut out,
        &mut index,
        laid.title.inset_by(pad),
        board.title().to_owned(),
        TITLE_FONT_SIZE,
        theme.text,
    );
    for column in &laid.columns {
        // ⚠ An id the layout carries and the board has lost yields an **empty label**, never
        // a `continue`. A `continue` here would skip the slot as well as the words, and every
        // label after it would shift by one — see the note in `push`. It cannot happen, since
        // the layout was built from this very board a few lines ago; writing it so it cannot
        // *matter* costs one `map_or_else` and removes the need to keep believing that.
        let label = board.column(column.id).map_or_else(String::new, |model| {
            match column.wip.limit {
                Some(limit) => format!("{}  {}/{}", model.title(), column.wip.count, limit),
                None => format!("{}  {}", model.title(), column.wip.count),
            }
        });
        push(
            &mut out,
            &mut index,
            column.header.inset_by(pad * 0.5),
            label,
            HEADER_FONT_SIZE,
            theme.text_muted,
        );
        for card in &column.cards {
            let words = board
                .card(card.id)
                .map_or_else(String::new, |model| model.label().to_owned());
            // Insetting rather than using the rect keeps the label off the card's rounded
            // corner — and it is the same padding the height was measured with, so the text
            // fits the box that was sized for it.
            push(
                &mut out,
                &mut index,
                FlowRect::new(
                    card.rect.left() + pad,
                    card.rect.top() + pad,
                    (card.rect.width() - pad * 2.0).max(1.0),
                    (card.rect.height() - pad * 2.0).max(1.0),
                ),
                words,
                CARD_FONT_SIZE,
                theme.text,
            );
        }
    }
    out
}

/// A widget-local rectangle in absolute world coordinates.
///
/// `origin` is the item's **unrotated** top-left, from [`Projected::rect`], and every layout
/// crate here reports offsets from exactly that corner.
fn world_rect(origin: WorldPoint, x: f64, y: f64, w: f64, h: f64) -> WorldRect {
    WorldRect::from_origin_size(
        WorldPoint::new(origin.x + x, origin.y + y),
        w.max(1.0),
        h.max(1.0),
    )
}

/// A slot index, saturating rather than wrapping.
///
/// A table with more than 65,535 cells would otherwise wrap two labels onto one key and draw
/// one of them twice. Saturating collapses the tail instead, which is a visible smudge on a
/// widget nobody can read at that size rather than a wrong word in a readable one.
fn slot_of(index: usize) -> u16 {
    u16::try_from(index).unwrap_or(u16::MAX)
}

// ---------------------------------------------------------------------------------------
// Measurement: the three bridges between a layout crate and the font stack.
// ---------------------------------------------------------------------------------------

/// A [`Measure`] backed by real shaping. `vellum-app`'s `table::ShapedMeasure`.
struct ShapedMeasure<'a> {
    engine: &'a mut TextEngine,
}

impl<'a> ShapedMeasure<'a> {
    fn new(engine: &'a mut TextEngine) -> Self {
        Self { engine }
    }

    /// A table cell's style as the text engine's parameters.
    ///
    /// `max_width` is set by the caller per question — `None` to ask how wide the text wants
    /// to be, `Some` to ask how tall it is at that width — which is exactly the two questions
    /// [`Measure`] asks.
    fn params(style: &TableTextStyle, max_width: Option<f32>) -> LayoutParams {
        LayoutParams {
            font_family: style.font_family.clone(),
            font_size: style.font_size as f32,
            line_height: style.line_height as f32,
            // Alignment moves glyphs inside a line and cannot change how much room the line
            // needs, so measuring always asks left-aligned.
            align: vellum_text::TextAlign::Left,
            max_width,
        }
    }
}

impl Measure for ShapedMeasure<'_> {
    fn intrinsic(&mut self, content: &TableText, style: &TableTextStyle) -> Intrinsic {
        let text = table_measure_text(content, style);
        // Unwrapped: every paragraph on one line, which is the widest the cell would ever
        // want to be.
        let max = self.engine.measure(&text, &Self::params(style, None));
        // Unbreakable: the widest single word, which is the narrowest the column can be
        // without a glyph hanging outside it. Measured by shaping the longest word alone
        // rather than by wrapping at 1px, because a wrap that cannot fit still reports the
        // overflowing line's full width and the two would come out equal.
        let longest = content
            .to_plain()
            .split_whitespace()
            .max_by_key(|word| word.chars().count())
            .map(str::to_owned)
            .unwrap_or_default();
        let min = if longest.is_empty() {
            0.0
        } else {
            let word = StyledText::plain(&longest);
            f64::from(self.engine.measure(&word, &Self::params(style, None)).width)
        };
        Intrinsic { min_content: min, max_content: f64::from(max.width).max(min) }
    }

    fn height(&mut self, content: &TableText, style: &TableTextStyle, width: f64) -> f64 {
        let text = table_measure_text(content, style);
        let extent = self
            .engine
            .measure(&text, &Self::params(style, Some(width.max(1.0) as f32)));
        // At least one line. `Measure`'s contract is explicit that an empty cell is still a
        // cell, and a row of them still has a height.
        let line = style.font_size * style.line_height;
        f64::from(extent.height).max(line)
    }
}

/// Chart label metrics backed by the real font stack. `vellum-app`'s `chart::ShapedMetrics`.
struct ShapedMetrics<'a> {
    engine: RefCell<&'a mut TextEngine>,
}

impl<'a> ShapedMetrics<'a> {
    fn new(engine: &'a mut TextEngine) -> Self {
        Self { engine: RefCell::new(engine) }
    }
}

impl vellum_chart::TextMetrics for ShapedMetrics<'_> {
    /// `TextMetrics::width` takes `&self` — measuring is conceptually a question, not a
    /// mutation — but shaping needs `&mut` on the engine, so the borrow is moved to runtime.
    /// Single-threaded and never re-entrant: `vellum-chart` calls this from its own layout
    /// and holds no borrow of its own across the call.
    fn width(&self, text: &str, size: f32) -> f32 {
        let params = LayoutParams { font_size: size, max_width: None, ..LayoutParams::default() };
        self.engine.borrow_mut().measure(&StyledText::plain(text), &params).width
    }
}

/// The map with every visible node's box measured from its shaped label.
///
/// Returns a copy: a measurement is a property of the font stack this process happens to
/// have, not something to write back to the document. `vellum-app`'s `mindmap::measured`.
fn measure_nodes(map: &MindMap, engine: &mut TextEngine) -> MindMap {
    let mut out = map.clone();
    for id in out.visible_nodes() {
        let Some(node) = out.get(id) else { continue };
        let style = node.style;
        let text = node_label(&node.text, &style);
        let params = LayoutParams {
            font_size: style.font_size as f32,
            max_width: None,
            ..LayoutParams::default()
        };
        let extent = engine.measure(&text, &params);
        // At least one line tall, whatever the label. An empty node collapsing to its
        // padding would be a sliver that cannot be read as a node.
        let line = style.font_size * f64::from(LayoutParams::default().line_height);
        let size = NodeSize::new(
            (f64::from(extent.width) + NODE_PADDING_X * 2.0).max(MIN_NODE_WIDTH),
            f64::from(extent.height).max(line) + NODE_PADDING_Y * 2.0,
        );
        if let Some(node) = out.get_mut(id) {
            node.size = size;
        }
    }
    out
}

/// The board with every card's height measured from its shaped label.
///
/// `vellum-app`'s `kanban::measured`, including the trick that keeps the two crates from
/// disagreeing about a column's width: rather than duplicating `vellum-flow`'s arithmetic,
/// the width is **read back from a first layout pass**. Cheap — that pass is pure arithmetic
/// over the cards' current heights, and those heights are the ones about to be replaced.
fn measure_cards(board: &Kanban, engine: &mut TextEngine, size: (f64, f64)) -> Kanban {
    let probe = board.layout(FlowRect::new(0.0, 0.0, size.0, size.1));
    let width = probe
        .columns
        .first()
        .map_or(size.0, |column| column.body.width())
        .max(1.0);
    let wrap = (width - CARD_PADDING * 2.0).max(1.0);

    let mut out = board.clone();
    let ids: Vec<vellum_flow::CardId> = out
        .columns()
        .iter()
        .flat_map(vellum_flow::kanban::Column::card_ids)
        .collect();
    for id in ids {
        let Some(card) = out.card(id) else { continue };
        let params = LayoutParams {
            font_size: CARD_FONT_SIZE as f32,
            max_width: Some(wrap as f32),
            ..LayoutParams::default()
        };
        let extent = engine.measure(&StyledText::plain(card.label()), &params);
        // At least the metrics' own default, so a one-word card is not shorter than the rest
        // and a board of them does not look ragged.
        let height = (f64::from(extent.height) + CARD_PADDING * 2.0).max(out.metrics().card_height);
        let _ = out.set_card_height(id, Some(height));
    }
    out
}

// ---------------------------------------------------------------------------------------
// Pure helpers. Everything below is arithmetic over values, with no engine and no list.
// ---------------------------------------------------------------------------------------

/// A mind map as the document stores it: the tree, plus the two choices that decide what
/// shape it is drawn in.
///
/// ⚠ **A hand-copy of `vellum-app`'s `mindmap::MindMapModel`,** which is the one token type
/// in this file that is not a layout crate's own — the other three decode straight into
/// [`Table`], [`ChartSpec`] and [`Kanban`]. The field names and the two `#[serde(default)]`
/// attributes are load-bearing and must match: `default` is what lets a map written before a
/// field existed still open, and a rename on either side silently degrades every map on every
/// board to the default layout. The durable fix is moving that type down into
/// `vellum-project`, which both front ends already depend on.
#[derive(Deserialize)]
struct MindMapModel {
    map: MindMap,
    #[serde(default)]
    kind: LayoutKind,
    #[serde(default)]
    connectors: ConnectorShape,
}

/// How much a map of this natural extent has to shrink to sit inside `size`.
///
/// Uniform, and the smaller of the two ratios: a map stretched to a dragged box would put its
/// text at one aspect and its branches at another. Resizing a mind map therefore scales it —
/// a tidy tree's extent is determined by the tree, not chosen. `vellum-app`'s
/// `mindmap::fit_scale`, and the drawing and the text slots both ask it rather than each
/// working it out, which is what keeps a node's words on the node.
fn fit_scale(natural: (f64, f64), size: (f64, f64)) -> f64 {
    let fit = (size.0 / natural.0).min(size.1 / natural.1);
    if fit.is_finite() && fit > 0.0 { fit } else { 1.0 }
}

/// A `vellum-table` colour as the renderer's.
///
/// Its own type rather than a shared one because `vellum-table` has no renderer dependency —
/// it is a layout crate, and `docs/01-architecture.md` keeps it that way.
fn table_colour(c: vellum_table::Rgba) -> Rgba {
    Rgba::from_rgb8(c.r, c.g, c.b).with_alpha(f32::from(c.a) / 255.0)
}

/// A table cell's spans as the text engine's, for **measuring**.
///
/// Bold and italic ride on the span in one model and on the *cell* in the other, so the
/// cell's own flags are folded into every span that does not already set them — which is what
/// `TextStyle`'s own documentation says they mean. Colour is dropped: it changes no metric,
/// and measuring is all this conversion is for.
fn table_measure_text(text: &TableText, style: &TableTextStyle) -> StyledText {
    StyledText::from_spans(
        text.spans()
            .iter()
            .map(|span| vellum_text::TextSpan {
                text: span.text.clone(),
                style: vellum_text::SpanStyle {
                    bold: span.style.bold || style.bold,
                    italic: span.style.italic || style.italic,
                    underline: span.style.underline,
                    strikethrough: span.style.strikethrough,
                    link: span.style.link.clone(),
                    color: None,
                },
            })
            .collect::<Vec<_>>(),
    )
}

/// A table cell's spans as the text engine's, for **drawing**.
///
/// Unlike [`table_measure_text`] this keeps each span's own colour, which is the whole
/// difference between the two: the drawing path wants it and the measuring path cannot use
/// it. `vellum-app`'s `table::to_text` draws the same line.
fn table_text(text: &TableText, style: &TableTextStyle) -> StyledText {
    StyledText::from_spans(
        text.spans()
            .iter()
            .map(|span| vellum_text::TextSpan {
                text: span.text.clone(),
                style: vellum_text::SpanStyle {
                    bold: span.style.bold || style.bold,
                    italic: span.style.italic || style.italic,
                    underline: span.style.underline,
                    strikethrough: span.style.strikethrough,
                    link: span.style.link.clone(),
                    color: span
                        .style
                        .color
                        .map(|c| vellum_text::Rgb { r: c.r, g: c.g, b: c.b }),
                },
            })
            .collect::<Vec<_>>(),
    )
}

/// A node's label as the text engine's spans.
///
/// Bold rides on the node's style in one model and on the span in the other, exactly as it
/// does for a table cell — so the flag is folded in here rather than at each of the two call
/// sites (measuring, and the text slot) that would otherwise have to remember.
fn node_label(text: &str, style: &NodeStyle) -> StyledText {
    StyledText::from_spans(vec![vellum_text::TextSpan {
        text: text.to_owned(),
        style: vellum_text::SpanStyle { bold: style.bold, ..vellum_text::SpanStyle::default() },
    }])
}

/// A chart polyline's points in the shared `[f32; 2]` vocabulary.
///
/// `vellum-chart` and `vellum-mindmap` each define their own point type, for the layering
/// reason those crates record, so the stroking below is written over the neutral one and each
/// caller converts. `vellum-app`'s `mesh.rs` makes the same choice.
fn polyline_points(line: &vellum_chart::Polyline) -> Vec<[f32; 2]> {
    line.points.iter().map(|p| [p.x, p.y]).collect()
}

/// A polyline as a triangle strip of the given width. `vellum-app`'s `mesh::ribbon`.
///
/// Segment quads with round-ish joins left implicit: at the widths a chart series or a
/// mind-map branch is drawn with, the gap at a join is sub-pixel, and mitring it properly
/// means solving the outer corner for every vertex. `vellum-ink` does that for freehand
/// strokes, where the width is large enough for it to show.
///
/// Degenerate input — fewer than two points, a repeated point, a non-positive width —
/// produces no triangles rather than `NaN`s. A repeated point in a data series is ordinary
/// data, not a bug to crash on.
fn ribbon(points: &[[f32; 2]], width: f32) -> Mesh {
    if points.len() < 2 || !width.is_finite() || width <= 0.0 {
        return Mesh::default();
    }
    let half = width / 2.0;
    let mut vertices = Vec::with_capacity((points.len() - 1) * 4);
    let mut indices = Vec::with_capacity((points.len() - 1) * 6);

    for pair in points.windows(2) {
        let ([ax, ay], [bx, by]) = (pair[0], pair[1]);
        let (dx, dy) = (bx - ax, by - ay);
        let length = dx.hypot(dy);
        if length <= f32::EPSILON {
            continue;
        }
        // The segment's normal, scaled to half the stroke.
        let (nx, ny) = (-dy / length * half, dx / length * half);
        let base = vertices.len() as u32;
        vertices.push([ax + nx, ay + ny]);
        vertices.push([ax - nx, ay - ny]);
        vertices.push([bx + nx, by + ny]);
        vertices.push([bx - nx, by - ny]);
        indices.extend_from_slice(&[base, base + 1, base + 2]);
        indices.extend_from_slice(&[base + 1, base + 3, base + 2]);
    }
    Mesh { vertices, indices }
}

/// A pie or donut slice as triangles. `vellum-app`'s `chart::slice_mesh`.
///
/// A donut is a quad strip between the two radii; a pie is the same loop with the inner
/// radius pinned to zero, so there is one path rather than two that must agree.
fn slice_mesh(arc: &vellum_chart::Arc) -> Mesh {
    let sweep = arc.sweep().abs();
    if sweep <= f32::EPSILON || arc.outer_radius <= 0.0 {
        return Mesh::default();
    }
    let steps = ((sweep.to_degrees() / ARC_STEP_DEGREES).ceil() as usize).max(1);
    let inner = arc.inner_radius.max(0.0).min(arc.outer_radius);

    let mut vertices = Vec::with_capacity((steps + 1) * 2);
    let mut indices = Vec::with_capacity(steps * 6);
    for step in 0..=steps {
        let t = step as f32 / steps as f32;
        let angle = arc.start_angle + (arc.end_angle - arc.start_angle) * t;
        let (sin, cos) = angle.sin_cos();
        vertices.push([arc.centre.x + cos * inner, arc.centre.y + sin * inner]);
        vertices.push([
            arc.centre.x + cos * arc.outer_radius,
            arc.centre.y + sin * arc.outer_radius,
        ]);
    }
    for step in 0..steps {
        let base = (step * 2) as u32;
        // Two triangles per step. For a pie the inner pair are all the centre, so the first
        // degenerates to zero area and costs a triangle rather than a branch.
        indices.extend_from_slice(&[base, base + 1, base + 3]);
        indices.extend_from_slice(&[base, base + 3, base + 2]);
    }
    Mesh { vertices, indices }
}

/// A closed ring as triangles, by fanning from its centroid. `vellum-app`'s
/// `chart::ring_mesh`.
///
/// Good enough for an **area** mark and only for that: `vellum-chart` builds those as a run
/// out along the values and back along the baseline, which is monotone in x and therefore
/// star-shaped about its own centroid. A general polygon needs a real tessellator, and this
/// must not be pointed at one.
fn ring_mesh(ring: &vellum_chart::Polyline) -> Mesh {
    let points = &ring.points;
    if points.len() < 3 {
        return Mesh::default();
    }
    let n = points.len() as f32;
    let (mut cx, mut cy) = (0.0f32, 0.0f32);
    for p in points {
        cx += p.x / n;
        cy += p.y / n;
    }

    let mut vertices = Vec::with_capacity(points.len() + 1);
    vertices.push([cx, cy]);
    vertices.extend(points.iter().map(|p| [p.x, p.y]));

    let mut indices = Vec::with_capacity(points.len() * 3);
    for i in 0..points.len() {
        let a = (i + 1) as u32;
        let b = ((i + 1) % points.len() + 1) as u32;
        indices.extend_from_slice(&[0, a, b]);
    }
    Mesh { vertices, indices }
}
