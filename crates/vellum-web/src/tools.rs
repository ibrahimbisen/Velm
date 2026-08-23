//! The tool palette, and the gesture that makes something.
//!
//! # What this is
//!
//! `edit.rs` gave the browser a selection, a move, a delete and an undo. This gives it the
//! other half of a board: a **create** gesture. A tool is armed from the page, a press
//! starts a placement, the drag previews it, and the release writes **one** undo step to
//! the document. It is `vellum_app::actions`' placing path — `default_size`,
//! `swept_placement`, `place`, `commit_stroke`, `draw_connector`, `erase_objects_at` —
//! reduced to what a tab can reach and no further.
//!
//! `web/tools.js` is the contract on the other side. Its `TOOL_GROUPS` and `MORE_TOOLS`
//! name fourteen ids and [`Tool::from_name`] answers those exact strings; its `EXPORTS`
//! table names `set_tool`, which is [`set_tool`] below.
//!
//! # ⚠ Two tools are refused, by name, and that is the honest answer
//!
//! `CLAUDE.md` feedback 36: *"never describe a gesture the user cannot perform."* Two of
//! the fourteen would produce an item this build can never finish:
//!
//! - **Text** makes an `ItemKind::Text` with empty words, and a text item with no words
//!   draws **nothing at all**. There is no on-canvas caret in a tab, so it could not be
//!   filled in here and it could not be found again — an invisible item somebody has to
//!   clean up on the desktop. A sticky is fine by the same test (a wordless sticky is a
//!   visible colour block), a shape is fine (it has a fill and an outline), and a frame is
//!   fine because it is born carrying the title `"Frame"`, exactly as the desktop's is.
//! - **Image** needs a file picker and a blob upload, and this crate has neither. The
//!   desktop says the same thing in as many words when its own Image tool is released.
//!
//! Both answer `false` from [`set_tool`] and are absent from [`tools_available`], so the
//! palette can grey them with the reason rather than arming a tool that does nothing.
//!
//! # ⚠ The preview and the created item are one decision
//!
//! [`swept_placement`] is called twice — once by [`ToolState::preview`] every frame of the
//! drag, once by [`ToolState::release`] — and never reimplemented. *"a preview that
//! disagrees with what it previews is worse than none, because it is believed."* The pen
//! goes further: [`stroke_placement`] is what both the live tessellation and the committed
//! item's centre are derived from, so a stroke does not shift the instant the button
//! comes up.
//!
//! # ⚠ A placing drag must draw while it is drawn
//!
//! This repository's most-repeated defect: state accumulated where the painter cannot see
//! it. The pen appeared to do nothing until release (feedback 7), then the frame did
//! (feedback 23), then an agent's prompt row did (feedback 34) — each written by somebody
//! who had read the previous entry. So the gesture lives in [`ToolState`], which is a
//! field on `Viewer`, and [`push_preview`] is one call the painter makes with a shared
//! borrow of it. There is no other copy of the in-flight state anywhere.
//!
//! # ⚠ The click floor is in device pixels, and this diverges from the desktop
//!
//! `vellum_app`'s `DRAG_TO_SIZE` is **8 world units**: a sweep smaller than that on either
//! axis is a click that wobbled, and the tool's default size is what was wanted. In a
//! browser that number is wrong at both ends, for the reason `edit.rs`'s `MOVE_FLOOR`
//! states outright — *"a slop expressed in world units is a different distance at every
//! zoom"*. On a board fitted at 4% one device pixel is twenty-five world units, so a tap
//! that wobbles three pixels sweeps seventy-five units on both axes and makes a 75×75
//! sticky instead of Miro's 199×228 — on a touchscreen, which is the device this port
//! exists for, on every tap. At 800% the inverse: a real drag of sixty-four device pixels
//! would still count as a click.
//!
//! So the threshold is 8 **device** pixels, divided by the zoom into world units
//! [`Gesture::Box::click_floor`] **at the press** and carried on the gesture. Captured
//! rather than recomputed so that a wheel zoom mid-gesture cannot give the preview and the
//! commit two different answers — which is the same property the shared function exists
//! for, and it would be quietly lost by reading `camera.zoom()` in both places.
//!
//! # ⚠ The undo group
//!
//! Trap 11, re-found three times: Loro's `group_start` answers `UndoGroupAlreadyStarted`
//! when a group is open, `group_end` merely clears the slot, there is no depth count, and
//! nothing else ever closes one — so a `?` escaping between begin and end breaks **every**
//! grouped operation for the rest of the session. There is no `Editor::edit` here, so
//! [`grouped`] is this file's chokepoint and it takes an **infallible** closure: there is
//! nothing to `?` on, so the failure is unrepresentable rather than recovered.
//!
//! It is the third copy of that function in this crate — `edit.rs` and `style.rs` each
//! have one, and the two already disagree about the begin-failure path (`edit.rs` closes
//! the leaked group and refuses; `style.rs` refuses without closing). This one follows
//! `edit.rs`: nothing in a tab holds a group deliberately across calls — there is no
//! caret, and the eraser below deletes once on release rather than per dab — so a group
//! that is already open is always a leak, and closing it is the rescue rather than a
//! theft of somebody's unfinished gesture. **The durable fix is one `pub(crate) fn
//! grouped` shared by all three**, and it is worth doing the next time one of them is
//! touched.
//!
//! # ⚠ Reproject after every change
//!
//! [`resettle`] runs on every path out of a mutation including the failure one. A closure
//! that failed part way has usually already changed the document, and a stale projection
//! does not merely draw the old board — it **hit-tests** the old board, so clicks land
//! where items used to be. `Editor::edit`'s hardest-won line, said here for the third time
//! in this crate.
//!
//! # Nothing here can be tested
//!
//! `vellum-web` is `#![cfg(target_arch = "wasm32")]`, so no test in this crate is ever
//! compiled. [`velm_tool_report`] is the honest check that is available, for the reason
//! `velm_edit_report` and `camera_report` exist: a fixture dispatches real pointer events
//! at the real listeners and then asks what happened. Trap 9's lesson — a fixture that
//! calls the handler directly starts downstream of everything that can go wrong.
//!
//! The pure decisions are free functions over plain values — [`swept_placement`],
//! [`stroke_placement`], [`decimate`] — so they can be moved to `vellum-project`, where
//! the tests run. `edit.rs` makes the same recommendation about its own three.

use std::cell::RefCell;
use std::rc::Rc;

use wasm_bindgen::prelude::*;

use vellum_doc::{
    ArrowKind, Board, ConnectorEnd, Dash, ItemId as DocId, ItemKind, NewItem, Placement, Routing,
    StyledText,
};
use vellum_project::project::Projection;
use vellum_project::theme::Theme;
use vellum_render::{DrawList, QuadInstance, Rgba, ShapeStyle};
use vellum_scene::{Camera, ItemId as SceneId, WorldPoint, WorldRect};

use crate::edit::Ring;

/// How far a placing drag must travel on **both** axes before it sizes the item it makes,
/// in **device pixels**.
///
/// `vellum_app`'s `DRAG_TO_SIZE` is this number in world units. See the module header for
/// why that cannot be carried across unchanged, and note that the desktop's *value* does
/// survive: at 100% zoom the two are byte for byte the same threshold.
const CLICK_FLOOR_PIXELS: f64 = 8.0;

/// The pen's width, in world units.
///
/// `vellum_ui::PenKind::Pen::default_width`. The browser has no pen flyout — there is no
/// nib, no colour and no translucency to choose — so this is the one number, and the
/// stroke's colour is left `None`, which every reader resolves to the theme's own ink.
/// That is deliberately not a copy of `color::INK`: a constant duplicated across a crate
/// boundary is the `THEME_BORDER` trap, and `None` cannot go stale.
const PEN_WIDTH: f64 = 4.0;

/// The object eraser's reach, in world units.
///
/// `vellum_app`'s `ERASER_MIN_RADIUS`, which on the desktop is a floor under the pen's
/// chosen width. With no pen flyout here it is simply the radius.
const ERASER_RADIUS: f64 = 6.0;

/// A connector's stroke width, in world units. `vellum_app`'s `CONNECTOR_THICKNESS`.
const CONNECTOR_THICKNESS: f64 = 2.0;

/// The shortest connector worth writing, in **device** pixels.
///
/// Two ends in the same place is not a connector, and silently making a one-unit one
/// leaves an invisible item on the board. In device pixels for [`CLICK_FLOOR_PIXELS`]'s
/// reason, and generously larger than it because a connector that lands on the item it
/// started from is worse than one that was refused.
const CONNECTOR_FLOOR_PIXELS: f64 = 12.0;

/// How thick the preview's outline is drawn, in **device** pixels.
///
/// The same shape as `SELECTION_RING_WIDTH` and for the same reason (feedback 32): an
/// outline that holds its device width reads as an outline at every zoom, where a
/// world-unit one is invisible at 4% and a slab at 8×. Two rather than one, because this
/// is a gesture in flight rather than a resting state — `SELECTION_WIDTH` keeps 2.0 on the
/// desktop for exactly the marquee, the placing preview and the pending connector.
const PREVIEW_WIDTH: f64 = 2.0;

// ---------------------------------------------------------------------------------------
// The tools
// ---------------------------------------------------------------------------------------

/// What the palette can arm.
///
/// The names are `web/tools.js`'s ids, and they are the wire: `mindmap` rather than
/// `mind_map`, `kanban` rather than `kanban_board`. A name this build does not know is
/// refused rather than silently mapped to Select — arming the wrong tool is worse than
/// arming none, because the next click makes something nobody asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Tool {
    #[default]
    Select,
    Hand,
    Sticky,
    Text,
    Shape,
    Frame,
    Pen,
    Eraser,
    Connector,
    Table,
    Chart,
    Kanban,
    MindMap,
    Image,
}

impl Tool {
    /// Every tool the palette draws, in the order `web/tools.js` draws them.
    pub const ALL: [Self; 14] = [
        Self::Select,
        Self::Hand,
        Self::Sticky,
        Self::Text,
        Self::Shape,
        Self::Frame,
        Self::Pen,
        Self::Eraser,
        Self::Connector,
        Self::Table,
        Self::Chart,
        Self::Kanban,
        Self::MindMap,
        Self::Image,
    ];

    /// The id `web/tools.js` uses, which is also what [`current_tool`] answers.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Select => "select",
            Self::Hand => "hand",
            Self::Sticky => "sticky",
            Self::Text => "text",
            Self::Shape => "shape",
            Self::Frame => "frame",
            Self::Pen => "pen",
            Self::Eraser => "eraser",
            Self::Connector => "connector",
            Self::Table => "table",
            Self::Chart => "chart",
            Self::Kanban => "kanban",
            Self::MindMap => "mindmap",
            Self::Image => "image",
        }
    }

    /// The tool a name asks for, or `None`.
    ///
    /// Exact match on the palette's own id. Not case-folded and not trimmed: this is a
    /// wire format between two files in one repository, and being lenient here would let a
    /// typo in the page look like a working button for as long as nobody pressed it.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|tool| tool.name() == name)
    }

    /// Why this build refuses to arm the tool, or `None` when it can.
    ///
    /// A sentence, because it is what the page shows. See the module header for the rule
    /// that produces this list and for why a wordless sticky is not on it.
    ///
    /// ⚠ **Text's refusal is conditional on something being built in parallel, and this is
    /// the line that reverses it.** `crates/vellum-web/src/caret.rs` is an on-canvas caret
    /// for the browser, with `caret::begin(viewer, scene, replacing)` — the same hop
    /// `vellum_app`'s `place` makes when it drops the cursor into a note it has just placed.
    /// The moment that module is in the tree, drop the `Text` arm below and add the
    /// begin-editing call to [`pointer_up`](self::pointer_up); the report that came with this
    /// file has both edits written out. Nothing else about Text needs changing —
    /// [`Tool::default_size`] and [`kind_for_tool`] are only missing an arm each. It is
    /// refused today because it is refused today, not because it is hard.
    #[must_use]
    pub const fn refusal(self) -> Option<&'static str> {
        match self {
            Self::Text => Some(
                "a text item is only its words, and this build has no on-canvas caret to \
                 write them with",
            ),
            Self::Image => Some("placing an image needs a file picker this build has not got"),
            _ => None,
        }
    }

    /// Whether the tool stays armed after it has been used.
    ///
    /// `vellum_ui::Tool::is_continuous`, and the reasoning is the desktop's: every create
    /// tool disarms so that a stray click cannot make an item nobody wanted, and the two
    /// drawing tools are the exception because for them the risk does not exist. A stray
    /// click with the pen makes **nothing** — [`stroke_placement`] wants two points before
    /// it keeps anything — and one with the eraser erases nothing. Nobody draws exactly
    /// one stroke.
    ///
    /// Select and Hand are continuous because they create nothing to disarm from.
    #[must_use]
    pub const fn is_continuous(self) -> bool {
        matches!(self, Self::Select | Self::Hand | Self::Pen | Self::Eraser)
    }

    /// The size this tool gives an item placed with a **click** rather than a drag.
    ///
    /// `vellum_app::ActiveState::default_size`, numbers included, so a sticky made in a tab
    /// is the same 199 × 228 as one made on the Mac and the two front ends do not draw
    /// subtly different boards.
    ///
    /// `None` for everything that makes nothing rectangular: Select and Hand, the pen and
    /// the eraser, which act on the board directly, the connector, which is a line between
    /// two things rather than a box, and Image, which this build refuses.
    #[must_use]
    pub const fn default_size(self) -> Option<(f64, f64)> {
        match self {
            // Miro's default sticky, and the size of every note on the reference board.
            Self::Sticky => Some((199.0, 228.0)),
            Self::Text => Some((240.0, 48.0)),
            // A square, so a first click with the shape tool gives a circle rather than an
            // ellipse — the desktop's own note, and it still applies here even though this
            // build's shape tool only makes rectangles.
            Self::Shape => Some((200.0, 200.0)),
            // 16:9, because a frame is a slide.
            Self::Frame => Some((1_600.0, 900.0)),
            // Miro's own insert: a 3 × 3 with a header row, in a box whose columns are
            // comfortably wider than a word.
            Self::Table => Some((480.0, 220.0)),
            Self::Chart => Some((480.0, 320.0)),
            // `vellum_app::kanban::DEFAULT_SIZE` and `mindmap::DEFAULT_SIZE`. Both are held
            // to their own content by a test **there** — a kanban that overflowed would
            // draw outside its own item, since `vellum-flow` lays overflow out past the
            // edge on purpose and nothing scrolls. Copied rather than derived because those
            // modules are in `vellum-app`, which does not compile for this target at all.
            Self::Kanban => Some((720.0, 420.0)),
            Self::MindMap => Some((520.0, 260.0)),
            Self::Select
            | Self::Hand
            | Self::Pen
            | Self::Eraser
            | Self::Connector
            | Self::Image => None,
        }
    }

    /// Whether a press with this tool armed belongs to the tool layer rather than to
    /// `edit.rs`.
    ///
    /// **Hand is in the list and creates nothing**, which is the point of it: claiming the
    /// press is how the tool stops `edit.rs` picking an item up, and with nothing picked up
    /// the one-finger drag in `input.rs` falls through to the camera pan. Without this the
    /// Hand tool would be indistinguishable from Select.
    #[must_use]
    const fn claims_the_press(self) -> bool {
        !matches!(self, Self::Select)
    }
}

// ---------------------------------------------------------------------------------------
// Pure decisions. No `Board`, no `Projection`, no `web_sys` — ready to move to
// `vellum-project`, where they can be tested.
// ---------------------------------------------------------------------------------------

/// The box a create tool would give the item it is about to make.
///
/// A sweep of at least `click_floor` **world** units on both axes fills the box it swept;
/// anything smaller is a click that wobbled, and a click makes [`Tool::default_size`]
/// centred on the press.
///
/// ⚠ **A [`Placement`] is a centre plus an extent, not a top-left.** Reading `x`/`y` as a
/// corner draws a frame half its own size up and to the left, which at 1600 × 900 is most
/// of a screen. `vellum_project::project::placement_bounds` is the function that converts
/// one to a rectangle, and the painter below goes through the same arithmetic the selection
/// rings do.
///
/// `click_floor` is passed in rather than read from a camera so that the preview and the
/// commit are handed the *same* number — see the module header.
#[must_use]
pub fn swept_placement(
    tool: Tool,
    from: WorldPoint,
    to: WorldPoint,
    click_floor: f64,
) -> Option<Placement> {
    let (width, height) = tool.default_size()?;
    let (swept_w, swept_h) = ((to.x - from.x).abs(), (to.y - from.y).abs());
    Some(if swept_w >= click_floor && swept_h >= click_floor {
        Placement::new((from.x + to.x) / 2.0, (from.y + to.y) / 2.0, swept_w, swept_h)
    } else {
        Placement::new(from.x, from.y, width, height)
    })
}

/// The box a pen stroke's points sit in, and the points made relative to its centre.
///
/// `None` for fewer than two points: a single sample is a click that happened to land
/// while the pen was held, not a mark. That is what makes a stray click with the pen make
/// nothing at all, which is [`Tool::is_continuous`]'s whole argument.
///
/// The points come back **relative to the placement**, which is what the document model
/// and the Miro importer both expect — so a stroke drawn in a tab and one imported from a
/// `.rtb` are indistinguishable afterwards, and moving the item moves the whole stroke.
///
/// ⚠ **Both the preview and the commit call this.** The live tessellation is drawn from
/// exactly the geometry that will be written, so a stroke cannot shift or rescale at the
/// moment the button comes up.
#[must_use]
pub fn stroke_placement(points: &[WorldPoint]) -> Option<(Placement, Vec<vellum_doc::Point>)> {
    if points.len() < 2 {
        return None;
    }
    let (mut min_x, mut min_y) = (f64::MAX, f64::MAX);
    let (mut max_x, mut max_y) = (f64::MIN, f64::MIN);
    for p in points {
        min_x = min_x.min(p.x);
        min_y = min_y.min(p.y);
        max_x = max_x.max(p.x);
        max_y = max_y.max(p.y);
    }
    if !(min_x.is_finite() && min_y.is_finite() && max_x.is_finite() && max_y.is_finite()) {
        return None;
    }
    let (cx, cy) = ((min_x + max_x) / 2.0, (min_y + max_y) / 2.0);
    let relative = points
        .iter()
        .map(|p| vellum_doc::Point { x: p.x - cx, y: p.y - cy })
        .collect();
    let placement = Placement {
        x: cx,
        y: cy,
        // A perfectly straight stroke has zero extent on one axis, and a zero-sized
        // placement is an item the R-tree cannot index and the hit-test can never find.
        width: (max_x - min_x).max(1.0),
        height: (max_y - min_y).max(1.0),
        ..Placement::default()
    };
    Some((placement, relative))
}

/// Whether a new pen sample is far enough from the last one to be worth keeping.
///
/// ⚠ **Half a *device* pixel, never half a world unit.** A slow hand emits hundreds of
/// sub-pixel moves and they add vertices without adding shape — but a flat world-unit floor
/// is the same filter only at 100% zoom. At 8× it is a four-device-pixel floor, so drawing
/// zoomed in, which is exactly what anyone does for detail, throws away most of the hand's
/// movement. And it is **destructive**: these are the points that get stored, so the
/// coarseness is baked into the document and no later re-render can recover it. That was a
/// real defect on the desktop (feedback 27) and the fix is this division.
///
/// Guarded against a nonsense zoom the same way `Lod`'s tolerance and the selection ring's
/// width are: a non-finite or non-positive zoom falls back to the unscaled floor rather
/// than producing an infinite threshold that would keep no points at all.
#[must_use]
pub fn decimate(last: Option<WorldPoint>, at: WorldPoint, zoom: f64) -> bool {
    let least = if zoom.is_finite() && zoom > 0.0 { 0.5 / zoom } else { 0.5 };
    last.is_none_or(|last| (at.x - last.x).hypot(at.y - last.y) > least)
}

/// A screen-pixel threshold as world units at this zoom.
///
/// One function, so every device-pixel constant in this file is converted the same way and
/// guarded the same way. See [`decimate`] for what an unguarded division does.
#[must_use]
fn world_units(pixels: f64, zoom: f64) -> f64 {
    if zoom.is_finite() && zoom > 0.0 { pixels / zoom } else { pixels }
}

// ---------------------------------------------------------------------------------------
// The gesture
// ---------------------------------------------------------------------------------------

/// The gesture in flight, if there is one.
///
/// Four shapes rather than one, because the four families genuinely differ in what they
/// accumulate: a box remembers two corners, a stroke remembers a path, an eraser remembers
/// what it has swept over, and a connector remembers two points and resolves what is under
/// them only at the release.
#[derive(Debug, Clone)]
enum Gesture {
    /// Sticky, shape, frame, table, chart, kanban, mind map.
    Box {
        tool: Tool,
        from: WorldPoint,
        to: WorldPoint,
        /// ⚠ Captured at the press. See the module header: recomputing it from the live
        /// zoom would let a wheel notch mid-drag give the preview and the commit two
        /// different answers about whether the gesture was a click.
        click_floor: f64,
    },
    /// The pen.
    Stroke { points: Vec<WorldPoint> },
    /// The object eraser. Nothing is written until the release.
    Erase {
        at: WorldPoint,
        /// What the sweep has passed over, in the order it was met. Scene ids, so they are
        /// pruned against the projection before they are used.
        doomed: Vec<SceneId>,
    },
    /// The connector.
    Line { from: WorldPoint, to: WorldPoint, floor: f64 },
}

/// The armed tool and the gesture in flight.
///
/// A field on `Viewer`, exactly as `EditState` is, and for the reason that module's header
/// gives: it holds no globals and reaches for nothing, so the whole of this file is a value
/// the viewer owns rather than a second piece of page state that could disagree with it.
#[derive(Debug, Default)]
pub struct ToolState {
    tool: Tool,
    gesture: Option<Gesture>,
}

impl ToolState {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub const fn tool(&self) -> Tool {
        self.tool
    }

    #[must_use]
    pub const fn is_placing(&self) -> bool {
        self.gesture.is_some()
    }

    /// Arms a tool. `false` when this build refuses it — see [`Tool::refusal`].
    ///
    /// ⚠ **Cancels whatever was in flight first.** Changing tool mid-sweep is one of the
    /// ways a gesture ends without a release, and it is the third route by which the
    /// desktop leaked an undo group (feedback 30, after Escape and a tab switch). Here it
    /// cannot leak one — no group is held open across calls — but the preview would
    /// otherwise outlive the tool that justified it, which is the same rule saying the same
    /// thing about a cheaper failure.
    pub fn arm(&mut self, tool: Tool) -> bool {
        if tool.refusal().is_some() {
            return false;
        }
        self.gesture = None;
        self.tool = tool;
        true
    }

    /// The gesture ended without a release — a second finger, `pointercancel`, the page
    /// being hidden, the tool being changed.
    ///
    /// Idempotent, and free when nothing is in flight. `CLAUDE.md` feedback 27's rule,
    /// which this repository has paid for on five separate gestures now: **give every way a
    /// gesture can end without a release a call to the function that closes it.**
    ///
    /// Takes no projection, unlike `EditState::cancel`, and that is a property of the
    /// design rather than luck: a placing preview writes nothing into the projection, so
    /// dropping it puts nothing back.
    pub fn cancel(&mut self) {
        self.gesture = None;
    }

    /// A press at a world point. Answers whether the tool layer claimed it.
    ///
    /// `false` means "not mine" and the caller should offer the press to `edit.rs` exactly
    /// as it does today. `true` covers **Hand**, which claims a press in order to create
    /// nothing: see [`Tool::claims_the_press`].
    ///
    /// A live gesture is cancelled first, for the reason `EditState::press` does the same —
    /// a `pointerup` the browser swallowed, or a system gesture, leaves one behind, and
    /// layering a second on top of it would preview two placements at once.
    pub fn press(&mut self, at: WorldPoint, zoom: f64) -> bool {
        self.gesture = None;
        // ⚠ Belt and braces against the one failure this module exists to avoid. `arm`
        // already refuses a tool this build has not got, so a refused tool cannot be in
        // `self.tool` — but `Tool::Text` **has** a `default_size`, so if one ever did get
        // in, the arm below would give it a full preview for an item `kind_for_tool`
        // silently declines to make. That is a described gesture the user cannot perform,
        // produced by the code meant to prevent it. Refused here as well, by construction.
        if self.tool.refusal().is_some() {
            return false;
        }
        if !self.tool.claims_the_press() {
            return false;
        }
        self.gesture = match self.tool {
            Tool::Pen => Some(Gesture::Stroke { points: vec![at] }),
            Tool::Eraser => Some(Gesture::Erase { at, doomed: Vec::new() }),
            Tool::Connector => Some(Gesture::Line {
                from: at,
                to: at,
                floor: world_units(CONNECTOR_FLOOR_PIXELS, zoom),
            }),
            tool if tool.default_size().is_some() => Some(Gesture::Box {
                tool,
                from: at,
                to: at,
                click_floor: world_units(CLICK_FLOOR_PIXELS, zoom),
            }),
            // Hand, and anything that claims the press without making a box: the press is
            // taken so `edit.rs` does not act on it, and no gesture is started so the
            // pointer move falls through to the camera.
            _ => None,
        };
        true
    }

    /// The pointer moved with the button down. Answers whether the tool layer consumed it.
    ///
    /// `zoom` is the live one, and it is used only for the pen's decimation — which *must*
    /// read the live value, since it is a filter on what the hand is doing right now rather
    /// than a decision captured at the press.
    pub fn drag_to(&mut self, at: WorldPoint, zoom: f64, projection: &Projection) -> bool {
        match self.gesture.as_mut() {
            Some(Gesture::Box { to, .. }) => {
                *to = at;
                true
            }
            Some(Gesture::Stroke { points }) => {
                if decimate(points.last().copied(), at, zoom) {
                    points.push(at);
                }
                true
            }
            Some(Gesture::Erase { at: where_, doomed }) => {
                *where_ = at;
                for id in erasable_under(at, projection) {
                    if !doomed.contains(&id) {
                        doomed.push(id);
                    }
                }
                true
            }
            Some(Gesture::Line { to, .. }) => {
                *to = at;
                true
            }
            None => false,
        }
    }

    /// The button came up. Answers what to write, if anything.
    ///
    /// Deliberately a value rather than a call, exactly as `edit::Commit` is: a gesture ends
    /// in `input.rs` where the borrows are a pointer handler's, and the write needs
    /// `&mut Board`. Handing back a description keeps the decision and the write in two
    /// places that can each be read on their own.
    ///
    /// **The tool disarms here**, for everything that is not continuous, and it happens
    /// whether or not the release produced anything — a create tool that stayed armed after
    /// a refused gesture is a stray click away from making the item it just declined to.
    pub fn release(&mut self) -> Option<Create> {
        let gesture = self.gesture.take()?;
        if !self.tool.is_continuous() {
            self.tool = Tool::Select;
        }
        match gesture {
            Gesture::Box { tool, from, to, click_floor } => {
                swept_placement(tool, from, to, click_floor)
                    .map(|placement| Create::Item { tool, placement })
            }
            Gesture::Stroke { points } => Some(Create::Stroke { points }),
            Gesture::Erase { doomed, .. } => {
                if doomed.is_empty() { None } else { Some(Create::Erase { doomed }) }
            }
            Gesture::Line { from, to, floor } => {
                // A tap rather than a drag: two ends in the same place is not a connector,
                // and silently making a one-unit one leaves an invisible item on the board.
                if (to.x - from.x).hypot(to.y - from.y) < floor {
                    log::info!("velm tools: a connector needs two ends — drag from one item to another");
                    return None;
                }
                Some(Create::Connector { from, to })
            }
        }
    }
}

/// What a released gesture asks the document to do.
#[derive(Debug, Clone, PartialEq)]
pub enum Create {
    Item { tool: Tool, placement: Placement },
    Stroke { points: Vec<WorldPoint> },
    Erase { doomed: Vec<SceneId> },
    Connector { from: WorldPoint, to: WorldPoint },
}

/// What an eraser dab at `at` may take.
///
/// Candidates come from the R-tree — the same index the painter culls with — and are then
/// filtered on the two rules the desktop's object eraser applies: **locked items survive**
/// (a lock means "I did not mean to touch this", and an eraser is exactly the accident it
/// exists to survive), and an item a frame has clipped out of the drawing is not erasable,
/// because something you cannot see must not be something you can destroy by sweeping over
/// where it is not.
///
/// Roots-only is deliberately *not* decided here: `edit::delete` already drops anything with
/// a doomed ancestor, and doing it twice would be two answers to one question.
fn erasable_under(at: WorldPoint, projection: &Projection) -> Vec<SceneId> {
    let reach = WorldRect::from_corners(
        WorldPoint::new(at.x - ERASER_RADIUS, at.y - ERASER_RADIUS),
        WorldPoint::new(at.x + ERASER_RADIUS, at.y + ERASER_RADIUS),
    );
    projection
        .scene()
        .query_rect(reach)
        .map(|item| item.id)
        .filter(|id| {
            projection.get(*id).is_some_and(|projected| {
                !projected.item.style.locked
                    && !vellum_project::frame::clipped_by_frame(projected, projection)
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------------------
// What the painter needs.
//
// One call, one shared borrow, and every geometry decision below is the same function the
// commit uses. See the module header on previews that disagree with what they preview.
// ---------------------------------------------------------------------------------------

/// How a placing preview is drawn.
///
/// Resolved from the tool **here** rather than in the painter, which draws the board and
/// knows nothing about a tool palette — `vellum_app::draw::PlacingLook`'s own split, and it
/// is what keeps `lib.rs`'s frame loop free of a `match` on a tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Look {
    /// Opaque white with a hairline edge — a frame previews as the thing it will be.
    Frame,
    /// The board's own sticky colour.
    Sticky,
    /// Through the same rectangle SDF the placed shape is drawn with.
    Shape,
    /// An accent ghost, for everything whose final look is not known until it exists: a
    /// table, a chart, a kanban and a mind map are all laid out from content that does not
    /// exist yet, and a wash is honest about that where a fake grid would not be.
    Ghost,
}

/// The gesture in flight, as the painter wants it.
#[derive(Debug, Clone)]
pub enum Preview {
    Box { placement: Placement, look: Look },
    Stroke { points: Vec<vellum_doc::Point>, centre: WorldPoint, thickness: f64 },
    Line { from: WorldPoint, to: WorldPoint },
    /// The eraser's disc, and a ring around everything the sweep has claimed so far.
    ///
    /// The rings are `edit::Ring`s so the painter draws them with the loop it already has
    /// for a selection rather than a second one that could come to disagree about how a
    /// rotated item's outline is placed.
    Erase { at: WorldPoint, radius: f64, doomed: Vec<Ring> },
}

impl ToolState {
    /// The gesture in flight, or `None`.
    ///
    /// Recomputed every frame rather than cached: a preview held between frames is state
    /// the painter reads and the gesture writes, which is one more thing that can go stale
    /// mid-drag — and all of this is a handful of arithmetic over at most a few hundred
    /// points.
    #[must_use]
    pub fn preview(&self, projection: &Projection) -> Option<Preview> {
        match self.gesture.as_ref()? {
            Gesture::Box { tool, from, to, click_floor } => {
                let placement = swept_placement(*tool, *from, *to, *click_floor)?;
                let look = match tool {
                    Tool::Frame => Look::Frame,
                    Tool::Sticky => Look::Sticky,
                    Tool::Shape => Look::Shape,
                    _ => Look::Ghost,
                };
                Some(Preview::Box { placement, look })
            }
            Gesture::Stroke { points } => {
                let (placement, relative) = stroke_placement(points)?;
                Some(Preview::Stroke {
                    points: relative,
                    centre: WorldPoint::new(placement.x, placement.y),
                    thickness: PEN_WIDTH,
                })
            }
            Gesture::Erase { at, doomed } => Some(Preview::Erase {
                at: *at,
                radius: ERASER_RADIUS,
                doomed: doomed
                    .iter()
                    .filter_map(|id| projection.get(*id))
                    .map(|projected| Ring {
                        centre: WorldPoint::new(
                            projected.item.placement.x,
                            projected.item.placement.y,
                        ),
                        size: projected.item.placement.scaled_size(),
                        rotation: projected.rotation(),
                    })
                    .collect(),
            }),
            Gesture::Line { from, to, .. } => Some(Preview::Line { from: *from, to: *to }),
        }
    }
}

thread_local! {
    /// How many draw-list entries the last `push_preview` added — read by `velm_tool_report`
    /// and by nothing else. A `Cell` rather than a field on `ToolState` because the painter
    /// holds that by shared reference, and widening it to `&mut` for one counter would put a
    /// mutable borrow through the whole paint pass.
    static DREW: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Draw whatever gesture is in flight.
///
/// ⚠ **The list must be in the *board* view.** Everything below is in camera-relative world
/// units, because the board view's clip transform already carries the zoom — multiplying by
/// `camera.zoom()` here applies it twice, which at a fitted 6% draws every preview at 6% of
/// its own box. `shapes.rs` and the image arm in `lib.rs` both carry the same warning, the
/// second with the measurement that produced it.
///
/// The one thing that *is* divided by the zoom is a **stroke width**, for the opposite
/// reason: a hairline sized in world units is invisible at 4% and a slab at 8×.
pub fn push_preview(
    list: &mut DrawList,
    camera: &Camera,
    tools: &ToolState,
    projection: &Projection,
    theme: &Theme,
) {
    let Some(preview) = tools.preview(projection) else {
        DREW.with(|count| count.set(0));
        return;
    };
    let before = list.pushed();
    let zoom = camera.zoom();
    let outline = (PREVIEW_WIDTH / zoom) as f32;

    match preview {
        Preview::Box { placement, look } => {
            // ⚠ A `Placement` is a centre. The top-left is where the quad goes, and it is
            // derived exactly as the selection rings' is, in `lib.rs`.
            let (width, height) = placement.scaled_size();
            let origin = camera.to_camera_relative(WorldPoint::new(
                placement.x - width / 2.0,
                placement.y - height / 2.0,
            ));
            let size = [width as f32, height as f32];
            match look {
                Look::Shape => {
                    // The same path a placed shape takes. This build's shape tool makes a
                    // rectangle and nothing else — there is no shape flyout in a tab — and
                    // `Shape::Rectangle` is analytic, so the preview costs one instance and
                    // no tessellation. A form that needed triangles would need a cache, and
                    // a cache is why this is the only shape the browser offers.
                    let extent = vellum_shapes::Size::new(size[0], size[1]);
                    if let Some(params) = vellum_shapes::Shape::Rectangle.sdf_params(extent) {
                        list.push_shape(
                            &params,
                            [origin[0] + size[0] / 2.0, origin[1] + size[1] / 2.0],
                            &ShapeStyle {
                                fill: theme.surface.with_alpha(theme.surface.a * 0.6),
                                border: theme.accent,
                                border_width: outline,
                                rotation: 0.0,
                                opacity: 1.0,
                            },
                        );
                    }
                }
                Look::Frame => {
                    // Opaque, and behind nothing: a frame is white and a preview that was a
                    // wash would read as a selection rather than as the thing being made.
                    list.push_quad(
                        QuadInstance::solid(origin, size, theme.frame_fill)
                            .with_border(theme.border, outline),
                    );
                }
                Look::Sticky => {
                    list.push_quad(
                        QuadInstance::solid(origin, size, theme.sticky)
                            .with_border(theme.accent, outline),
                    );
                }
                Look::Ghost => {
                    list.push_quad(
                        QuadInstance::solid(origin, size, theme.accent.with_alpha(0.10))
                            .with_border(theme.accent, outline),
                    );
                }
            }
        }
        Preview::Stroke { points, centre, thickness } => {
            // Tessellated fresh every frame, with no cache. A live stroke is a few hundred
            // points and it changes on every one of them, so a cache keyed on anything
            // would miss every time and cost a hash lookup for the privilege.
            let coordinates: Vec<(f64, f64)> = points.iter().map(|p| (p.x, p.y)).collect();
            // ⚠ **The same band `strokes.rs` tessellates a committed stroke at**, so the
            // preview and the item it previews are flattened to the same tolerance and a
            // stroke does not visibly change shape at the moment the button comes up. The
            // item's scale is not in the product because a stroke being drawn has none yet
            // — a new `Placement` is scale 1.0.
            let mesh = vellum_ink::Stroke::from_miro(&coordinates, Some(thickness))
                .render(vellum_ink::Lod::new(2f64.powi(vellum_project::look::lod_band(zoom))))
                .unwrap_or_else(|error| {
                    // Logged, not swallowed: "the pen draws nothing" and "the pen is not
                    // wired up" look identical on screen, and the one line that tells them
                    // apart costs a console write on a path that does not normally run.
                    log::warn!("velm tools: previewing a stroke: {error}");
                    vellum_ink::Mesh::default()
                });
            if mesh.indices.is_empty() {
                return;
            }
            // The points are relative to the stroke's own centre — the same centre the
            // committed item will have — so the transform carries the board's absolute
            // extent and the vertices never do. At 41,282 world units wide that is the
            // difference between `f32` holding the geometry and not.
            let transform = list.meshes_mut().push_transform(
                vellum_render::MeshTransform::at(camera.to_camera_relative(centre)),
            );
            let start = list.meshes().indices().len() as u32;
            list.meshes_mut().push_ink(&mesh, theme.stroke, transform);
            let end = list.meshes().indices().len() as u32;
            if end > start {
                list.push_meshes(start..end);
            }
        }
        Preview::Line { from, to } => {
            // A rotated quad rather than a routed path: the route depends on what the two
            // ends bind to, and that is not resolved until the release. A straight line
            // between the fingers is what the gesture actually promises.
            let (dx, dy) = (to.x - from.x, to.y - from.y);
            let length = dx.hypot(dy);
            if length <= 0.0 {
                return;
            }
            let width = CONNECTOR_THICKNESS.max(world_units(PREVIEW_WIDTH, zoom));
            let size = [length as f32, width as f32];
            // Rotation is about the quad's own centre — the convention the selection rings
            // already rely on — so the origin is the midpoint less half the extent.
            let origin = camera.to_camera_relative(WorldPoint::new(
                (from.x + to.x) / 2.0 - length / 2.0,
                (from.y + to.y) / 2.0 - width / 2.0,
            ));
            list.push_quad(
                QuadInstance::solid(origin, size, theme.accent)
                    .with_rotation(dy.atan2(dx) as f32),
            );
        }
        Preview::Erase { at, radius, doomed } => {
            // Everything the sweep has claimed. Drawn before the disc, so the pointer's own
            // mark is on top of them.
            //
            // ⚠ **In the accent, which on this board means *selected* rather than *going
            // away*, and that is a named gap rather than a choice.** Feedback 22 split those
            // two roles deliberately — *"delete looked like selected"* — and gave destruction
            // its own red. That red is `vellum_ui`'s, and `vellum_project::theme::Theme`
            // carries no destructive token at all, so this crate cannot reach it. The
            // alternative is `text_muted`, which on a near-white board is a grey ring on a
            // grey board: invisible is worse than ambiguous when the thing being said is
            // "these are about to go". A `danger` field on `Theme` is the fix, and it would
            // serve both front ends.
            for ring in doomed {
                let origin = camera.to_camera_relative(WorldPoint::new(
                    ring.centre.x - ring.size.0 / 2.0,
                    ring.centre.y - ring.size.1 / 2.0,
                ));
                let mut wash = theme.accent;
                wash.a = 0.12;
                list.push_quad(
                    QuadInstance::solid(origin, [ring.size.0 as f32, ring.size.1 as f32], wash)
                        .with_border(theme.accent, outline)
                        .with_rotation(ring.rotation),
                );
            }
            // A **disc**, through the same analytic ellipse a placed shape would use. A
            // square outline here would read as a marquee — the one other thing on this
            // canvas drawn as a thin box under the pointer — and the whole job of this mark
            // is to say what the sweep is about to take.
            let origin = camera
                .to_camera_relative(WorldPoint::new(at.x - radius, at.y - radius));
            let side = (radius * 2.0) as f32;
            let extent = vellum_shapes::Size::new(side, side);
            if let Some(params) = vellum_shapes::Shape::Ellipse.sdf_params(extent) {
                list.push_shape(
                    &params,
                    [origin[0] + side / 2.0, origin[1] + side / 2.0],
                    &ShapeStyle {
                        fill: Rgba::TRANSPARENT,
                        border: theme.text_muted,
                        border_width: outline,
                        rotation: 0.0,
                        opacity: 1.0,
                    },
                );
            }
        }
    }

    // ⚠ **What was actually pushed, not what was asked for.** `is_placing` says a gesture is
    // live and says nothing about whether anything reached the screen — and *that* is the
    // defect this application has now shipped three times (feedback 7 the pen, 23 the frame,
    // 34 the agent prompt): state accumulating where the painter cannot see it, so the tool
    // appears to do nothing until the button comes up. A fixture reading `is_placing` alone
    // would have passed through all three.
    DREW.with(|count| count.set(list.pushed().saturating_sub(before)));
}

// ---------------------------------------------------------------------------------------
// Document operations.
// ---------------------------------------------------------------------------------------

/// Runs `f` inside one undo group, and closes the group on **every** path.
///
/// The third copy of this function in this crate; see the module header for why it exists
/// separately today and what the durable fix is. It takes an **infallible** closure, so
/// trap 11's failure — a `?` escaping between begin and end — is unrepresentable rather
/// than recovered.
fn grouped<T>(board: &mut Board, f: impl FnOnce(&mut Board) -> T) -> Result<T, String> {
    if let Err(error) = board.begin_undo_group() {
        // Not `?`. A group already open is a leak from somewhere, and closing it is the
        // whole point of being here: the alternative is a board on which no grouped
        // operation works again until the tab is reloaded. The next operation succeeds.
        board.end_undo_group();
        return Err(format!("could not start an undo step: {error}"));
    }
    let value = f(&mut *board);
    board.end_undo_group();
    Ok(value)
}

/// Rebuilds the projection from the document, logging rather than failing.
///
/// ⚠ **Called on every path out of a mutation, including the failure one.** See the module
/// header: a stale projection hit-tests the old board, so clicks land where items used to
/// be.
fn resettle(board: &Board, projection: &mut Projection) {
    if let Err(error) = projection.rebuild(board) {
        log::error!("velm tools: laying the board out again: {error}");
    }
}

/// Adds one item in one undoable step and answers its document id.
///
/// Grouped even though it is a single `add`, because a single `add` is several Loro
/// operations — the node, its placement, its kind — and without the group `⌘Z` would undo
/// a placement and leave an empty node behind.
fn add_one(
    board: &mut Board,
    projection: &mut Projection,
    kind: ItemKind,
    placement: Placement,
) -> Result<DocId, String> {
    let added = grouped(&mut *board, |board| {
        board.add(NewItem::new(kind, placement)).map_err(|error| error.to_string())
    });
    resettle(board, projection);
    added?
}

/// What each tool makes.
///
/// `None` for the tools that make nothing rectangular; the caller has already established
/// that a placement exists, and the two answer `Some` for exactly the same set — this pairs
/// them without either answering for the other, which is `place`'s own arrangement on the
/// desktop.
fn kind_for_tool(tool: Tool) -> Option<ItemKind> {
    match tool {
        // `background: None` means "the board theme's default", not "transparent", so a
        // sticky made here is the same yellow one made on the Mac is. There is no sticky
        // colour flyout in a tab to read a choice from.
        Tool::Sticky => Some(ItemKind::Sticky { text: StyledText::default(), background: None }),
        // ⚠ A rectangle, and only a rectangle. There is no shape flyout in this build, so
        // there is no choice to read — and the preview above draws the same form through the
        // same analytic path, which is what keeps the two agreeing.
        Tool::Shape => Some(ItemKind::Shape {
            form: encode_token(&vellum_shapes::Shape::Rectangle, "shape"),
            text: StyledText::default(),
        }),
        // The title is a placeholder, exactly as the desktop's is — and here it is doing
        // more work than there: it is the only thing that makes a new frame visible at a
        // fitted zoom, and this build has no caret to replace it with.
        Tool::Frame => Some(ItemKind::Frame {
            title: StyledText::plain("Frame"),
            order: None,
            speaker_notes: None,
        }),
        Tool::Table => Some(ItemKind::Table { model: encode_token(&default_table(), "table") }),
        Tool::Chart => Some(ItemKind::Chart { spec: encode_token(&default_chart(), "chart") }),
        Tool::Kanban => Some(ItemKind::Kanban { board: encode_token(&default_kanban(), "kanban") }),
        Tool::MindMap => Some(ItemKind::MindMap { model: default_mindmap_token() }),
        Tool::Select
        | Tool::Hand
        | Tool::Text
        | Tool::Pen
        | Tool::Eraser
        | Tool::Connector
        | Tool::Image => None,
    }
}

/// One of the document's opaque tokens.
///
/// ⚠ **An empty string on failure, which every reader in both front ends degrades from.**
/// `widgets.rs` draws a placeholder for an empty token and says so in the console;
/// `shapes.rs` draws a rectangle. Losing the contents is bad and losing the *item* is
/// worse, which is the rule the whole document layer follows.
fn encode_token<T: serde::Serialize>(value: &T, what: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|error| {
        log::warn!("velm tools: a {what} would not encode ({error}); storing nothing");
        String::new()
    })
}

/// Miro's own insert: a 3 × 3 with a header row.
///
/// `vellum_app::table::default_table`. The header row is what makes a table read as a table
/// rather than as a grid of boxes.
fn default_table() -> vellum_table::Table {
    let mut table = vellum_table::Table::new(3, 3);
    table.set_header_rows(1);
    table
}

/// `vellum_app::chart::default_chart` — a two-series bar chart over four quarters.
fn default_chart() -> vellum_chart::ChartSpec {
    use vellum_chart::{ChartKind, ChartSpec, Dataset, Series};
    let data = Dataset::new(
        ["Q1", "Q2", "Q3", "Q4"],
        vec![
            Series::new("Actual", vec![12.0, 19.0, 15.0, 24.0]),
            Series::new("Target", vec![15.0, 15.0, 20.0, 20.0]),
        ],
    );
    ChartSpec::categorical(ChartKind::bar(), data)
}

/// `vellum_app::kanban::default_kanban` — three columns, six cards, one WIP limit.
///
/// Two cards against a limit of two: full, but not breached. A default that shipped already
/// over its own limit would read as an error state.
fn default_kanban() -> vellum_flow::Kanban {
    let mut board = vellum_flow::Kanban::new("Sprint");
    let todo = board.add_column("To do");
    let doing = board.add_column("Doing");
    let done = board.add_column("Done");
    let _ = board.set_wip_limit(doing, Some(2));
    for label in ["Measure the atlas", "Cull by band", "Fold the branches"] {
        let _ = board.add_card(todo, label);
    }
    for label in ["Shape the labels", "Route the connectors"] {
        let _ = board.add_card(doing, label);
    }
    let _ = board.add_card(done, "Pin the breakdown");
    board
}

/// A mind map's token: `vellum_app::mindmap::default_mindmap`'s tree, encoded by hand.
///
/// ⚠ **The wrapper object is written directly rather than through a fourth copy of
/// `MindMapModel`.** That type is `vellum-app`'s, and `widgets.rs` already carries a
/// hand-copy of it with a note explaining that the copy is the price of the type living in
/// the wrong crate. A *third* copy — a `Serialize` one, here — would be one more thing to
/// keep in step with a rename nobody would notice. Both existing readers mark `kind` and
/// `connectors` `#[serde(default)]`, which is exactly what makes `{"map": …}` a complete
/// token: it decodes to the default layout and the default connector shape on the desktop
/// and in this tab alike.
///
/// The **styling** is deliberately not copied. The desktop's default map paints white nodes
/// with a frost border and a teal root, from four constants in `vellum-app`; those are the
/// accent-dependent tokens `CLAUDE.md` records as living in five places of which three are
/// checked, and adding a sixth unchecked copy is worse than a map drawn in
/// `vellum_mindmap::NodeStyle`'s own defaults. What is lost is a fill and a border, not the
/// tree — and a map made here can be restyled on the desktop.
fn default_mindmap_token() -> String {
    use vellum_mindmap::{MindMap, Node};
    let mut map = MindMap::with_root(Node::new("Central idea"));
    let centre = map.root();
    for (title, leaves) in [
        ("Branch one", ["Detail", "Another detail"].as_slice()),
        ("Branch two", ["Detail"].as_slice()),
        ("Branch three", ["Detail", "Another detail"].as_slice()),
    ] {
        let Ok(parent) = map.add_child(centre, Node::new(title)) else { continue };
        for leaf in leaves {
            let _ = map.add_child(parent, Node::new(*leaf));
        }
    }
    encode_token(&serde_json::json!({ "map": map }), "mind map")
}

/// Writes a connector between whatever the two ends landed on.
///
/// `vellum_app::draw_connector`, less the port pin — there are no anchor dots on the canvas
/// in a tab, so every end is resolved from where the pointer went. Four rules, each the
/// desktop's and each earned by the case that goes wrong without it:
///
/// - **A connector never binds to another connector.** `ConnectorEnd::target` names an
///   item, and routing one to another would need a point on a path rather than on a box.
/// - **The far end must not bind to the item the drag started on.** A line from a sticky to
///   itself is drawn straight through the box, which reads as a rendering fault rather than
///   as a loop anybody asked for.
/// - **Each anchor faces the *other* end**, measured against the other item's centre when
///   there is one and against the raw pointer when there is not. A release lands *inside*
///   the target roughly always, so measuring to the release point joins two side-by-side
///   boxes top-to-top as often as right-to-left.
/// - **The arrowhead is on the far end only**, which is what a connector drawn by dragging
///   means: the direction is the direction of the drag.
fn add_connector(
    board: &mut Board,
    projection: &mut Projection,
    from: WorldPoint,
    to: WorldPoint,
) -> Result<DocId, String> {
    // ⚠ **Scoped, and the shared reborrow is named.** `projection` arrives as `&mut`, so a
    // closure that reached for it directly would capture the mutable borrow and then be
    // alive across the write below. `edit.rs` names the same `let view: &Projection` for the
    // same reason; the block is what guarantees the borrow is over before `add_one` runs
    // rather than leaving it to where the last use happens to fall.
    let (start_item, end_item) = {
        let view: &Projection = projection;
        // `hit_test_where`, never `hit_test` then a filter: `scene.rs` states the rule
        // verbatim — the predicate is applied *before* "topmost", so a rejected item is
        // genuinely transparent. Filtered afterwards, a clipped card lying over a sticky
        // does not merely fail to bind, it **swallows** the end and the sticky underneath
        // never gets it.
        let under = |at: WorldPoint| -> Option<(DocId, Placement)> {
            let scene = view.scene().hit_test_where(at, |id| {
                view.get(id).is_some_and(|projected| {
                    !matches!(projected.item.kind, ItemKind::Connector { .. })
                        && !vellum_project::frame::clipped_by_frame(projected, view)
                })
            })?;
            let projected = view.get(scene)?;
            Some((projected.doc_id, projected.item.placement))
        };
        let start_item = under(from);
        let end_item =
            under(to).filter(|(doc, _)| start_item.is_none_or(|(held, _)| held != *doc));
        (start_item, end_item)
    };

    let toward = |item: Option<&(DocId, Placement)>, fallback: WorldPoint| {
        item.map_or((fallback.x, fallback.y), |(_, p)| (p.x, p.y))
    };
    // The connector's own box. Only the **free** ends are normalised against it: a bound
    // end's anchor is a fraction of its *target's* box and does not care what this is.
    let own = vellum_project::connector::placement_for((from.x, from.y), (to.x, to.y));
    let start = match &start_item {
        Some((doc, placement)) => ConnectorEnd::bound(
            *doc,
            vellum_project::connector::facing_anchor(placement, toward(end_item.as_ref(), to)),
        ),
        None => ConnectorEnd::free(vellum_project::connector::free_anchor(
            &own,
            (from.x, from.y),
        )),
    };
    let end = match &end_item {
        Some((doc, placement)) => ConnectorEnd::bound(
            *doc,
            vellum_project::connector::facing_anchor(placement, toward(start_item.as_ref(), from)),
        ),
        None => ConnectorEnd::free(vellum_project::connector::free_anchor(&own, (to.x, to.y))),
    }
    .with_arrowhead(ArrowKind::FilledTriangle);

    add_one(
        board,
        projection,
        ItemKind::Connector {
            start,
            end,
            routing: Routing::default(),
            dash: Dash::default(),
            thickness: CONNECTOR_THICKNESS,
            color: None,
            captions: Vec::new(),
        },
        own,
    )
}

// ---------------------------------------------------------------------------------------
// The hops `input.rs` calls — one line each, so the file this module does not own stays
// almost unchanged, and so the decisions stay here where they can be read together.
// ---------------------------------------------------------------------------------------

/// A pointer went down on the canvas. Answers whether the **tool** layer claimed it.
///
/// `true` means the caller must **not** offer the press to `edit.rs`. That covers every
/// create tool and it covers **Hand**, which claims a press in order to leave the camera
/// pan as the only thing that happens.
///
/// ⚠ **Gated on `edit.enabled()`, which is the one switch.** Creating an item is editing,
/// and until the push half is trusted an edit in a tab is volatile — `edit.rs`'s header
/// makes the whole argument. With editing off this answers `false` for every tool, so
/// wiring it changes nothing about a read-only page.
pub fn pointer_down(viewer: &mut crate::Viewer, at: WorldPoint) -> bool {
    if !viewer.edit.enabled() {
        return false;
    }
    let zoom = viewer.camera.zoom();
    viewer.tools.press(at, zoom)
}

/// The pointer moved with the button down. Answers whether the tool layer consumed it.
///
/// `false` when nothing is in flight — which is the common case, and the Hand tool's case,
/// so the caller falls through to the camera pan.
pub fn pointer_move(viewer: &mut crate::Viewer, at: WorldPoint) -> bool {
    let zoom = viewer.camera.zoom();
    let crate::Viewer { tools, projection, .. } = viewer;
    tools.drag_to(at, zoom, projection)
}

/// The pointer came up. Writes whatever the gesture asks for, and answers whether the
/// document changed.
///
/// `true` also suppresses `input.rs`'s badge-opening tap, which is right: a gesture that
/// just made an item is not a request to open the page of whatever was underneath it.
pub fn pointer_up(viewer: &mut crate::Viewer) -> bool {
    let Some(what) = viewer.tools.release() else { return false };
    // ⚠ **Editing can be switched off *between* the press and the release, and this is the
    // only place that can refuse the write.** `velm_set_editing(false)` cancels `edit.rs`'s
    // gesture and knows nothing about this one, and on a touchscreen the sequence is
    // ordinary rather than exotic: one finger mid-drag on the canvas, another on the page's
    // own switch. Without this an item lands on a board whose editing was just turned off.
    //
    // `release` has already run, so the gesture is dropped and the tool disarmed exactly as
    // it would have been — the one thing left to refuse is the document write. That is
    // feedback 27's rule at the point where the gesture's *contract* ends rather than where
    // the pointer does.
    if !viewer.edit.enabled() {
        return false;
    }
    let crate::Viewer { board, projection, edit, push, .. } = viewer;

    let outcome: Result<Option<DocId>, String> = match what {
        Create::Item { tool, placement } => match kind_for_tool(tool) {
            Some(kind) => add_one(board, projection, kind, placement).map(Some),
            // Unreachable today — `kind_for_tool` and `Tool::default_size` answer `Some` for
            // exactly the same set — and an early return rather than a panic, because
            // `panic = "abort"` on wasm turns a mistake here into a dead tab.
            None => Ok(None),
        },
        Create::Stroke { points } => match stroke_placement(&points) {
            Some((placement, relative)) => add_one(
                board,
                projection,
                ItemKind::Ink {
                    points: relative,
                    // The theme's own ink. See `PEN_WIDTH` on why this is not a constant.
                    color: None,
                    thickness: PEN_WIDTH,
                },
                placement,
            )
            .map(Some),
            // A stray click with the pen. Nothing is written and nothing is reported: this
            // is the behaviour `Tool::is_continuous` is built on, not a failure.
            None => Ok(None),
        },
        Create::Erase { doomed } => {
            // ⚠ **`edit::delete`, rather than a second removal path here.** It already owns
            // the three rules — locked items survive, roots only (a frame takes its subtree
            // and `Board::remove` refuses an id its own previous call removed), and the
            // count is what *left the board* rather than how many removals were asked for.
            // It also opens the group at `edit.rs`'s own chokepoint, so a sweep is one undo
            // step and this file's `grouped` is not involved.
            //
            // This diverges from the desktop, which erases live per dab inside a group held
            // open for the whole sweep — the arrangement that leaked a group three separate
            // ways (Escape, a tab switch, changing tool). Deleting once on release cannot,
            // and the preview above is what makes the sweep visible in the meantime.
            match crate::edit::delete(board, projection, &doomed) {
                Ok(count) => {
                    edit.sync(projection);
                    return finish(count > 0, board, push);
                }
                Err(error) => Err(error),
            }
        }
        Create::Connector { from, to } => add_connector(board, projection, from, to).map(Some),
    };

    match outcome {
        Ok(Some(doc)) => {
            // ⚠ **After `resettle`, never before.** `scene_id` reads the projection, and the
            // projection does not know the new item until it has been rebuilt — which
            // `add_one` has already done by the time this runs.
            if let Some(scene) = projection.scene_id(doc) {
                edit.select_only(scene);
            }
            edit.sync(projection);
            finish(true, board, push)
        }
        Ok(None) => false,
        Err(error) => {
            log::error!("velm tools: {error}");
            false
        }
    }
}

/// Tells the push half there is something to send, and sends it this frame.
///
/// ⚠ **After the mutation and after the reprojection, never before** — `Pusher::note_edit`'s
/// own documentation states it: `tick` exports the board as it stands, so a `note_edit` that
/// ran first and a `tick` that ran before the write would send an empty delta and mark the
/// real edit acknowledged.
///
/// Silently nothing when there is no server behind this board: a static `board.bin` has
/// nowhere to push to, and that is the ordinary development case rather than an error.
fn finish(changed: bool, board: &Board, push: &mut Option<crate::push::Pusher>) -> bool {
    if changed && let Some(pusher) = push.as_mut() {
        pusher.note_edit();
        pusher.tick(board);
    }
    changed
}

/// The gesture ended without a release. Idempotent, and free when nothing is in flight.
pub fn pointer_cancel(viewer: &mut crate::Viewer) {
    viewer.tools.cancel();
}

// ---------------------------------------------------------------------------------------
// The exports the page calls.
// ---------------------------------------------------------------------------------------

/// The viewer, if the page has one and nothing further up the stack is holding it.
///
/// Every borrow is a **`try_`** borrow. `panic = "abort"` is set for every release profile
/// including `web`, so a `RefCell` collision here is not a caught panic and a line in the
/// console — it is a dead tab with nothing on screen and nothing in the log. `None` means
/// "not now", which for a button press is the right answer.
fn viewer() -> Option<Rc<RefCell<crate::Viewer>>> {
    crate::VIEWER.with(|slot| slot.try_borrow().ok().and_then(|held| held.clone()))
}

/// Arms a tool by the name `web/tools.js` calls it. `false` when it was not armed.
///
/// Three ways this answers `false`, and the page should treat all three the same way — the
/// button does not become pressed:
///
/// - the board is not ready, or a frame is holding the viewer;
/// - the name is not one of the fourteen;
/// - this build refuses the tool. See [`Tool::refusal`] and [`tools_available`], which is
///   the export a palette should grey its rows from at mount time rather than discovering
///   the refusal on a click.
///
/// ⚠ **Arming does not require editing to be on, and using the tool does.** The palette is
/// drawn on a read-only page too, and a tool that refused to arm there would say "this
/// build cannot draw" about a build that can — the honest thing to say is "this board is
/// not editable", which `can_edit()` already says. The gate is at
/// [`pointer_down`](self::pointer_down), which is one place rather than fifteen.
#[wasm_bindgen]
pub fn set_tool(name: &str) -> bool {
    let Some(tool) = Tool::from_name(name) else {
        log::warn!("velm tools: no tool called `{name}`");
        return false;
    };
    if let Some(why) = tool.refusal() {
        log::info!("velm tools: {} is not in this build — {why}", tool.name());
        return false;
    }
    let Some(held) = viewer() else { return false };
    let Ok(mut viewer) = held.try_borrow_mut() else { return false };
    viewer.tools.arm(tool)
}

/// The armed tool's name.
///
/// ⚠ **The page must read this rather than remember what it last asked for.** A create tool
/// **disarms itself** the moment it has placed something — Miro's behaviour and the
/// desktop's, so that a stray click on a canvas where a stray click is otherwise free
/// cannot make an item nobody wanted. A palette that tracked its own state would go on
/// showing Sticky pressed while Select was actually armed, and the next drag would sweep a
/// marquee while the interface promised a note.
///
/// `"select"` before the board is ready, which is what a board opens in.
#[wasm_bindgen]
pub fn current_tool() -> String {
    viewer()
        .and_then(|held| held.try_borrow().ok().map(|viewer| viewer.tools.tool().name().to_owned()))
        .unwrap_or_else(|| Tool::Select.name().to_owned())
}

/// The tools this build will arm, space-separated.
///
/// ⚠ **This exists so the palette can be honest before it is pressed.** `CLAUDE.md`
/// feedback 36: *"never describe a gesture the user cannot perform"*, and *"a disabled
/// control with a tooltip naming what is missing is the house style"*. Two of the fourteen
/// are refused here — see the module header — and without this the page would draw them
/// enabled and discover the refusal only on a click, which is the worse half of the same
/// mistake.
///
/// Space-separated rather than JSON, matching `velm_history_state` and `camera_report`: a
/// caller that splits on whitespace cannot be broken by a change of punctuation.
#[wasm_bindgen]
pub fn tools_available() -> String {
    Tool::ALL
        .iter()
        .filter(|tool| tool.refusal().is_none())
        .map(|tool| tool.name())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Why a tool is not in this build, or an empty string when it is.
///
/// The sentence a tooltip should carry. Separate from [`tools_available`] because a list
/// says *which* and this says *why*, and a page that had only the list would have to invent
/// the reason — which is how a build ends up apologising vaguely for something it could
/// have named.
#[wasm_bindgen]
pub fn tool_refusal(name: &str) -> String {
    Tool::from_name(name)
        .and_then(Tool::refusal)
        .unwrap_or("")
        .to_owned()
}

/// The tool layer, as a string, for a fixture to read.
///
/// The only honest check available in a crate where **no test is ever compiled**. It exists
/// for the reason `velm_edit_report` and `camera_report` exist: `input.rs` is driven by DOM
/// events, so the only way to verify a gesture is to dispatch real events at the real
/// listeners and then ask what happened. Trap 9's lesson in a fourth place — a fixture that
/// calls the handler directly starts downstream of everything that can go wrong between the
/// browser and the handler, and stays green on a build where the listener was never
/// attached.
///
/// `tool placing points doomed items`, space-separated:
///
/// - `tool` — what is armed *now*, which after a placement is `select`;
/// - `placing` — 1 while a gesture is in flight, so a fixture can photograph the **preview**
///   by stopping without releasing. That is the assertion feedback 7, 23 and 34 each needed
///   and did not have: a build that previewed nothing and a build that previewed correctly
///   are indistinguishable from the finished item alone;
/// - `points` — how many samples the pen has kept, which is what tells a working decimation
///   from a filter that is throwing the hand's movement away;
/// - `doomed` — how many items the eraser has swept over but not yet removed;
/// - `drawn` — how many draw-list entries the **last painted frame's** preview added, which
///   is the assertion `placing` cannot make: a gesture can be perfectly live and reach the
///   painter through nothing at all;
/// - `items` — the projection's item count, so a creation and an undo can each be measured
///   against it.
#[wasm_bindgen]
pub fn velm_tool_report() -> String {
    let Some(held) = viewer() else { return "select 0 0 0 0 0".to_owned() };
    let Ok(viewer) = held.try_borrow() else { return "select 0 0 0 0 0".to_owned() };
    let (points, doomed) = match viewer.tools.gesture.as_ref() {
        Some(Gesture::Stroke { points }) => (points.len(), 0),
        Some(Gesture::Erase { doomed, .. }) => (0, doomed.len()),
        _ => (0, 0),
    };
    format!(
        "{} {} {} {} {} {}",
        viewer.tools.tool().name(),
        u8::from(viewer.tools.is_placing()),
        points,
        doomed,
        viewer.projection.len(),
        DREW.with(std::cell::Cell::get)
    )
}
