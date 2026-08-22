//! Turning the visible slice of a board into a [`DrawList`].
//!
//! This is the frame's hot path and the only place that knows how every
//! [`vellum_doc::ItemKind`] looks. Its shape is set by three constraints:
//!
//! **Cost follows the viewport.** Nothing here iterates the document. The R-tree
//! hands back what is on screen, sorted by paint order, and the loop below runs over
//! exactly that — which is why a 596-item board and a 60-item board cost the same to
//! pan around.
//!
//! **Paint order is the document's, and it is not negotiable.** Items are emitted
//! strictly back to front, one at a time, geometry then text. Grouping all the fills
//! and then all the text would coalesce into fewer draw calls, and it would also
//! draw a sticky's label on top of the frame that is supposed to be covering it.
//! [`DrawList`] coalesces consecutive same-kind draws by itself, and a real board is
//! run-structured enough that it gets most of that saving anyway.
//!
//! **Text is drawn in screen pixels.** `vellum_text` rasterises a glyph at the size
//! it will occupy on screen and snaps its baseline to the pixel grid; pushing the
//! result through the camera transform would scale it a second time and undo both.
//! So a block's world origin is projected to device pixels here and the glyphs go
//! into a screen-space view. It is also why text below
//! [`MIN_DEVICE_FONT_SIZE`](crate::text::MIN_DEVICE_FONT_SIZE) is not rasterised at
//! all: at the zoom that fits the whole reference board, 429 text strings rasterise
//! to sub-pixel smudges at real cost.
//!
//! **Text too small to read is greeked, not dropped.** Below that threshold a block
//! draws as a bar per line — [`Painted::Greeked`] — in the board view, so a zoomed-out
//! board still shows *where* its words are. Dropping it instead left a board that is
//! full looking empty, worst of all for `ItemKind::Text`, which has no geometry of its
//! own to fall back on. The bars are quads, so they cost less than the glyphs they
//! replace and stay in the batch the item's own geometry already opened.
//!
//! # Known gaps
//!
//! - **Frames clip at item granularity, not per pixel.** An item wholly outside its
//!   frame is dropped; one straddling the edge is drawn whole. A scissor rect is the
//!   real answer and needs `vellum_render::Renderer::draw` to accept one.
//! - **Text on a rotated item is drawn upright**, centred on the item. Glyph
//!   instances carry no rotation, so this needs either a rotated glyph pipeline or
//!   a per-block transform in the text view.
//! - **`vellum_doc::ItemKind` has no shape variant**, so the SDF path in
//!   [`push_shape_card`] is currently reached only by the card kinds. Every Miro
//!   shape is already in `vellum-shapes`; nothing here changes when the document
//!   grows the variant except the match arm that dispatches to it.

use std::collections::HashMap;
use std::time::Duration;
use crate::time::Instant;

use vellum_agent::{DisplayMode, Status, TranscriptEvent};
// `vellum_agent::filetree::View` and `vellum_render::View` are both `View`, and this file
// uses the second one on every frame. Renamed on the way in rather than written out at each
// use, so the one that means "a draw list's coordinate space" keeps the bare name.
use vellum_agent::filetree::View as TreeView;
use vellum_connect::Router;
use vellum_doc::{
    Align, CardMode, ItemKind, Pattern, Style, StyledText as DocText, TextSpan as DocSpan,
};
use vellum_ink::{Lod, Stroke};
// ⚠ **Not defined here any more, and that is the point.** These are the measurements the
// browser front end needs too, and it had its own copies carrying the same values — which is
// the arrangement this repository has paid for three times, because two copies never disagree
// on the day they are written. `vellum_project::look` owns them; both painters read them.
pub(crate) use vellum_project::look::grid_step;
// ⚠ Not defined here any more, and this one was the *third* time — a link card's type scale,
// the air between its three voices, and the two string rules that decide what its title
// actually says. Every one of these was arrived at by the user photographing a Velm card beside
// Miro's, and several of them twice, so a browser card setting its title at a different scale
// from the Mac's is exactly the failure `look`'s header names at length.
//
// ⚠ **These were *copied* into `vellum_project::card`, not moved**, and this file went on
// carrying a private duplicate of all fourteen for a whole round — which is the arrangement
// the shared crate exists to prevent, arriving inside the change that created it. The copies
// were verified code-identical before being deleted; two of them (`strip_site_affix`,
// `says_the_same_as`) have aborted the application on real board data, and the tests that pin
// those aborts now live beside the one copy of the code rather than beside one of two.
use vellum_project::card::{
    BLURB_BOTTOM_AIR, BLURB_LINES, BLURB_SCALE, CARD_LINE_HEIGHT, PROVIDER_GAP, PROVIDER_SCALE,
    TITLE_GAP, TITLE_LINES, TITLE_SCALE, ellipsise, estimated_lines, line_budget,
    says_the_same_as, strip_site_affix, whole_lines,
};
// ⚠ Not defined here any more. An item hidden by the frame it has left is a rule both
// painters must apply identically or the two applications draw different boards — so it
// moved down beside `look`, where it also gained the tests it never had in this file.
use vellum_project::frame::clipped_by_frame;
use vellum_project::look::{
    CARD_PADDING, GRID_DOT, MAX_GRID_DOTS, cover_uv, frame_title_size, lod_band,
};
use vellum_render::{
    DrawList, DrawStats, GlyphAtlas, ImageInstance, MeshTransform, QuadInstance, Renderer, Rgba,
    ShapeStyle, View,
};
use vellum_scene::{Camera, ItemId as SceneId, ScreenPoint, WorldPoint};
use vellum_shapes::{Shape, Size, TessellationOptions as ShapeTessellation};
use vellum_text::{FitBox, GlyphImage, GlyphKey};

use crate::agent::{AgentLayout, Rect as NodeRect};
use crate::agent_view::AgentView;
use crate::assets::{self, Assets};
use crate::connector;
use crate::project::{Projected, Projection};
use crate::text::{self, BlockKey, MIN_DEVICE_FONT_SIZE, TextCache};
use crate::theme::{self, Theme};

/// Corner radius of a sticky note, in world pixels. Miro's notes are square-ish with
/// a small softening; `docs/05-design-language.md` §2 rules out pillowy radii.
const STICKY_RADIUS: f32 = 4.0;

/// Corner radius of a card — link preview, embed, document.
const CARD_RADIUS: f32 = 6.0;

/// Hairline width for borders, in world pixels. One device pixel at 100%.
const HAIRLINE: f32 = 1.0;

/// Gesture and handle chrome width, in **device** pixels: all of it is chrome and
/// must stay the same thickness however far the board is zoomed.
///
/// This sizes the resize/rotate handles' outline, the multi-selection box, the marquee,
/// the placing preview, the pending connector and the kanban drop placeholder. It no
/// longer sizes the ring around a selected item — that is [`SELECTION_RING_WIDTH`], which
/// is deliberately half of this.
const SELECTION_WIDTH: f32 = 2.0;

/// The ring around a selected item, in **device** pixels — half the weight of the rest of
/// the selection chrome above, at the user's request: *"the border on the selected items
/// are too thick"*.
///
/// A ring is constant on screen while the item it wraps shrinks with the zoom, so on a
/// fitted board a 2px ring is a band *around* a small item rather than an outline *on* it.
/// One device pixel is the `HAIRLINE` idiom this design already uses everywhere it would
/// otherwise reach for a shadow.
///
/// **No test can see this value.** `DrawList` exposes no quad reader, so
/// `a_selection_ring_holds_its_screen_width_at_any_zoom` asserts quad *counts*; the honest
/// check is a `--screenshot` of `--demo shapes --select-all`, measured in pixels.
const SELECTION_RING_WIDTH: f32 = 1.0;

/// How long one frame may spend shaping text it has not laid out before.
///
/// Opening the reference board asks for 236 text layouts at once, and Miro's auto-fit
/// binary-searches a dozen shaping passes for each of them. Paying that in one frame
/// is a visible stall on the very first thing the user sees. Spending a slice per
/// frame instead means the board appears immediately and its labels fill in over the
/// next few frames — the same policy images already follow, and for the same reason.
/// Everything already laid out is free and unaffected.
///
/// **The bound is "budget plus one block", not "budget".** Shaping cannot be
/// interrupted, so the check is made before each block rather than during it, and a
/// single expensive one can overrun. Measured on the reference board that puts the
/// worst frame of a cold open at 12 ms against a 3 ms budget — the overrun is one
/// link card auto-fitting into a 4000 px box, which stops being a text layout at all
/// once the importer emits `ItemKind::LinkPreview` instead of substituting text.
const TEXT_LAYOUT_BUDGET: Duration = Duration::from_millis(3);

// A frame's title size is `vellum_project::look::frame_title_size`, shared with the browser
// painter. It used to be three constants here, under a comment claiming that *"Miro scales a
// frame's name with the frame rather than with the zoom"* — offered as the reason no device
// floor was needed. **That was wrong**, and it is the belief that produced the bug: the user
// photographed both applications and Miro's frame names were legible where Velm's were a
// smear. Miro scales with the frame *and clamps*. The floor below is the half that stayed,
// because it is about this painter's cache and a caret, neither of which a reader has.

/// The smallest a frame's title is ever **drawn**, in device pixels.
///
/// A frame's name is what tells you where you are on a board you have zoomed out of, which
/// makes it precisely the text that must survive the zoom-out — and before this it was the one
/// thing on the canvas that could not. At the default frame size it greeked below 18.5% zoom,
/// and a greeked frame title is `GreekLines::Single`: one grey bar as wide as the whole frame.
///
/// Well clear of `MIN_DEVICE_FONT_SIZE` (5.0), which is the point — both greek guards read the
/// clamped scale, so a frame title can no longer reach either of them.
///
/// It floors magnification only. Zoom *in* and the world size takes over again, so a title
/// still grows with its frame and this constant becomes inert; see `Block::scale`.
const FRAME_TITLE_MIN_DEVICE: f64 = 13.0;

/// Fraction of a line's height a greeked bar occupies — roughly an x-height, so a
/// stack of them has the visual weight of the text it replaces rather than reading
/// as a solid block.
const GREEK_BAR_FRACTION: f64 = 0.45;

/// A greeked bar is never thinner than this on screen, in **device** pixels.
///
/// Without the floor a bar is as sub-pixel as the glyphs it stands in for, which
/// defeats the point. A quad's coverage is analytic where a glyph's rasterisation is
/// not, so one device pixel of bar is a crisp line and one device pixel of glyph is
/// noise — that asymmetry is the whole reason this feature works.
const GREEK_MIN_DEVICE_HEIGHT: f64 = 1.0;

/// Line-to-line spacing below which a stack of bars is mush, in **device** pixels.
/// Under this the block collapses to a single bar spanning its whole extent.
const GREEK_LINE_SPACING_MIN: f64 = 2.0;

/// Greeked text is a mark saying "words are here", not a headline. Muting it keeps a
/// zoomed-out board reading as a board rather than as a barcode.
const GREEK_ALPHA: f32 = 0.55;

/// Everything a frame needs to know that is not the document.
pub struct DrawContext<'a> {
    pub camera: &'a Camera,
    pub projection: &'a Projection,
    pub theme: Theme,
    /// Items drawn with a selection ring.
    pub selection: &'a [SceneId],
    /// The card whose ↗ open badge the pointer is on, if any.
    ///
    /// *"add a hover animation to the outgoing links so when i hover over this i can see
    /// that i am actually hovering over this"*. The badge is the one thing on the board
    /// that is a *button* rather than an object — clicking it leaves the app — and it had
    /// no hover state at all, so nothing distinguished "about to open a web page" from
    /// "about to select a card". Resolved by the app, which owns the hit test the click
    /// itself uses, rather than re-derived here: two copies of a hitbox is a click landing
    /// where the paint is not, which is the rule `card_layout` already exists to enforce.
    pub hovered_badge: Option<SceneId>,
    /// The item wearing Miro's four connector ports — the blue dots you drag a line from.
    ///
    /// Resolved by the app, never here, for `hovered_badge`'s reason and one more: *which*
    /// item wears them depends on the hover, the selection, the armed tool and whether a
    /// gesture is in flight, and the painter is allowed to read none of those.
    /// [`crate::actions::ActiveState::ported_item`] holds the whole rule.
    ///
    /// `None` on nearly every frame of an ordinary board, which is what keeps this free.
    pub ports: Option<SceneId>,
    /// A selected connector's two ends, in **world** units, as the grips that re-attach it.
    ///
    /// Resolved by the app through `crate::connector::endpoints` — the same function the
    /// router resolves with — because the painter would otherwise have to look up both
    /// targets itself and could then disagree with the line it is drawing about where the
    /// line begins.
    pub connector_grips: Option<((f64, f64), (f64, f64))>,
    /// The chat theme a node with no choice of its own draws in — Preferences ▸ Agents.
    ///
    /// Resolved by the app because it lives in the library sidecar, which `vellum-agent`
    /// must not know about and the painter cannot read. `DisplayMode`'s arrangement exactly.
    pub default_chat_theme: vellum_agent::ChatTheme,
    /// The display mode a node with no choice of its own is drawn in — the same arrangement,
    /// for the same reason, and it was the arrangement that was missing.
    ///
    /// ⚠ Two places resolved `None` against `DisplayMode::default()` — the enum's own value,
    /// which is not the user's. Both are reached only by a node the runtime has not attached a
    /// session to, so with the app-wide default set to Raw a freshly placed agent drew its
    /// toggle in Clean until the moment it first ran, and then changed under the user. The
    /// value follows `default_chat_theme` from the same sidecar so the two cannot drift.
    pub default_display: DisplayMode,
    /// The marquee in flight, in physical pixels.
    pub marquee: Option<(ScreenPoint, ScreenPoint)>,
    /// The pen stroke being drawn right now, if the button is down.
    ///
    /// A stroke becomes an item only when the button comes up — see
    /// [`crate::actions::ActiveState::commit_stroke`] for why nothing is written
    /// before then — so until it does, this is the only thing that can put it on
    /// screen. Without it the pen draws nothing at all until the gesture ends.
    pub stroke: Option<LiveStroke<'a>>,
    /// The item a create tool is sweeping out right now.
    ///
    /// The same reasoning as the pen's live stroke, arrived at from the other end and
    /// reported separately: *"when i am trying to draw a frame i do not see it as i draw …
    /// it just spawns"*. A placing drag writes nothing to the document until the button
    /// comes up, so without this the board is unchanged for the whole gesture and the item
    /// appears from nowhere at the end of it.
    pub placing: Option<Placing>,
    /// The agents running on this board, as the painter sees them.
    ///
    /// A snapshot filled once per frame by `crate::agent_runtime`, never the sessions
    /// themselves — see [`crate::agent_view`] for why the painter is deliberately given
    /// something it cannot start a turn with. Empty on every board that has no agent nodes,
    /// which is what keeps this layer free for boards that do not use it.
    pub agents: &'a crate::agent_view::AgentViews,
    /// The alignment guides the gesture in flight is reporting, in **world** units.
    ///
    /// Miro's *Align objects*. Resolved by the app together with the correction they
    /// explain, never re-derived here: a guide computed separately from the snap it
    /// describes is a line pointing at somewhere the item did not go.
    pub guides: &'a [crate::snap::Guide],
    /// The board's own background pattern, `docs/05-design-language.md` §4's optional
    /// grid among them. Drawn in `frost` over whatever the canvas is cleared to.
    pub pattern: Pattern,
    /// What the pattern is drawn in, when the board has chosen — colour *and* alpha.
    ///
    /// `None` means [`Theme::grid`], which is the near-black the user asked for at the
    /// contrast feedback 27 tuned. Resolved by the app rather than read from the board
    /// here for the reason `canvas_color` already is: one function decides the board's
    /// paint, so the minimap and the canvas cannot come to disagree about it.
    pub grid_color: Option<vellum_doc::Color>,
    /// The connector being drawn, as two **world** points.
    ///
    /// Like the pen's live stroke and the kanban card's drop preview: a connector becomes an
    /// item only when the button comes up, so until then this is the only thing that puts
    /// the gesture on screen.
    pub pending_connector: Option<(WorldPoint, WorldPoint)>,
    /// The on-canvas caret and selection, when a text slot is being edited.
    ///
    /// The words travel with it — see [`TextCursor`] for why the painter is told the string
    /// rather than reading it back off the item.
    pub editing: Option<TextCursor<'a>>,
    /// Where a kanban card being dragged would land, as four **world** corners.
    ///
    /// Corners rather than a rectangle because the item can be rotated and the
    /// placeholder has to sit on the column it is previewing. Like the pen's live
    /// stroke, this is the only thing that puts the gesture on screen: a card drag
    /// writes nothing to the document until the button comes up, so without it the
    /// drag is invisible and the card appears to jump on release.
    pub card_drop: Option<[(f64, f64); 4]>,
    /// Every orchestrator's region, and the one being swept right now.
    ///
    /// Resolved by the app — see [`crate::actions::ActiveState::territories`] — for `guides`'
    /// reason: whose region it is, whether it is the selected one and whether a gesture is
    /// redrawing it are questions about the selection and about `Input`, neither of which the
    /// painter is allowed to read. **Empty on every board with no orchestrator on it**, which
    /// is the early-out the two loops below both take.
    pub territories: Vec<TerritoryTint>,
    /// Where the minimap goes, in physical pixels — `[x, y, width, height]`. `None`
    /// hides it. Supplied by the caller rather than derived here because it has to
    /// clear the floating chrome, and only the caller knows where that is.
    pub minimap: Option<[f32; 4]>,
}

/// An orchestrator's region, ready to paint.
///
/// Owned rather than borrowed, like [`Placing`]: it is rebuilt each frame from a selection
/// and a live gesture, and the `label` is a `String` composed from the node's own text.
#[derive(Debug, Clone, PartialEq)]
pub struct TerritoryTint {
    /// Centre and extent in **world** units — `[x, y, width, height]`, the shape
    /// [`vellum_agent::Territory`] and `vellum_doc::Placement` both are, so nothing has to
    /// convert between a corner and a centre on the way here. That conversion is exactly
    /// where an off-by-half-a-box error lives.
    pub region: [f64; 4],
    /// Whose region it is, as the node is named on the board.
    ///
    /// **This is the answer to *"the orchestrator for that area should somehow be
    /// visible"***. With every region drawn at once rather than only the selected one, the
    /// chip is the only thing on screen saying which of several rectangles belongs to which
    /// node — so it stopped being decoration the moment the list grew past one.
    pub label: String,
    /// The node itself. Used as the text cache's key, so a label is shaped once and retired
    /// with the item — see [`crate::text::TextCache::retain`] — and to find the selected
    /// node's own region.
    pub scene: SceneId,
    /// The same node's document id, which is what an armed sweep remembers.
    ///
    /// Both are carried because they answer different questions and neither derives the
    /// other cheaply: a `SceneId` is rebuilt by every reprojection, and the arm has to
    /// survive one.
    pub doc: vellum_doc::ItemId,
    /// The projection generation the label was composed against, so a renamed node reshapes
    /// and a panning board does not.
    pub generation: u64,
    /// How loudly this one is drawn.
    pub emphasis: TerritoryEmphasis,
}

/// Which of the three things a region on screen is.
///
/// One enum rather than two booleans, because the three are exclusive and a pair of flags
/// admits a fourth state — *swept but not selected* — that nothing can produce and that the
/// alpha table would have to invent an answer for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerritoryEmphasis {
    /// A region that is simply there. The faintest, and the commonest: with every region
    /// drawn at all times this is what a board full of orchestrators looks like, and
    /// anything louder would recolour the canvas.
    Standing,
    /// Its owner is the current selection — *"this is the one you are looking at"*.
    Selected,
    /// The rectangle under the pointer, right now. The strongest, because during the one
    /// gesture where it matters most which rectangle is which, the answer must not be
    /// "they look the same".
    Sweeping,
}

impl TerritoryEmphasis {
    /// The wash and the edge, as a pair, so the two can never be looked up separately and
    /// come from different rungs.
    const fn alphas(self) -> (f32, f32) {
        match self {
            Self::Standing => (TERRITORY_FILL_ALPHA_STANDING, TERRITORY_EDGE_ALPHA_STANDING),
            Self::Selected => (TERRITORY_FILL_ALPHA, TERRITORY_EDGE_ALPHA),
            Self::Sweeping => (TERRITORY_FILL_ALPHA_LIVE, TERRITORY_EDGE_ALPHA_LIVE),
        }
    }
}

impl TerritoryTint {
    /// The region as two screen corners, in physical pixels, top-left first.
    ///
    /// One function, asked by both the paint and the label's placement, for the rule
    /// `card_layout` and `draw::kanban_runs` already exist to enforce: a second copy of a
    /// rectangle's arithmetic is a label that sits where the tint is not.
    fn screen_rect(&self, camera: &Camera) -> ((f32, f32), (f32, f32)) {
        let [x, y, width, height] = self.region;
        let a = camera.world_to_screen(WorldPoint::new(x - width / 2.0, y - height / 2.0));
        let b = camera.world_to_screen(WorldPoint::new(x + width / 2.0, y + height / 2.0));
        (
            (a.x.min(b.x) as f32, a.y.min(b.y) as f32),
            (a.x.max(b.x) as f32, a.y.max(b.y) as f32),
        )
    }
}

/// The territory label, laid out and placed. Screen pixels throughout.
struct TerritoryLabel {
    key: BlockKey,
    /// The text block's top-left, which is what `PlacedGlyph::physical` adds its offsets to.
    origin: (f32, f32),
    /// The plate behind it — `[x, y, width, height]`.
    plate: [f32; 4],
}

/// The slot a territory's label is cached under, on the node that owns the region.
///
/// **The top of the space, and it is safe by construction rather than by luck.** Every other
/// slot comes from `0..Painter::slots_of(..)`, which is exclusive and whose own arithmetic
/// saturates at `u16::MAX` — so the largest slot any item can ever use is `u16::MAX - 1`.
/// Keying on the node's own `SceneId` is what makes the label retire with the node:
/// `TextCache::retain` drops every entry whose item has left the board, and a synthetic id
/// would be a layout that leaked for the life of the process.
const TERRITORY_LABEL_SLOT: u16 = u16::MAX;

/// The label's size in **device** pixels — see [`Painter::territory_label`] for why this is
/// not a world size.
const TERRITORY_LABEL_SIZE: f32 = 12.0;
/// Air around the label's words, in device pixels.
const TERRITORY_LABEL_PAD: f32 = 6.0;
/// How far the plate sits inside the region's corner, in device pixels.
const TERRITORY_LABEL_INSET: f32 = 8.0;
/// The plate's corner radius. A hair rounder than a hairline, so it reads as a chip.
const TERRITORY_LABEL_RADIUS: f32 = 4.0;

/// How much of the accent a stored region washes the board with.
///
/// A **wash**, not a tint you could mistake for a fill: a territory routinely covers most of
/// the board, and anything strong enough to read as an object at that size would recolour
/// the whole canvas. Read against the near-white `paper` this design settled on, the edge is
/// what says where the region is and the wash only says which side of it you are on.
const TERRITORY_FILL_ALPHA: f32 = 0.05;
/// The same, while the rectangle is being swept. Stronger, because the gesture is the one
/// moment the fill *is* the feedback.
const TERRITORY_FILL_ALPHA_LIVE: f32 = 0.10;
/// The same, for a region whose owner is not selected — which since the regions became
/// always-on is every region on the board nearly all of the time.
///
/// **Half the selected wash, not the same.** A territory routinely covers most of the board
/// and there may be several; at the selected value they stack into a recoloured canvas, and
/// the middle rung of [`TerritoryEmphasis`] stops meaning anything.
const TERRITORY_FILL_ALPHA_STANDING: f32 = 0.025;
/// The dashed edge, against a board of stickies. Above a guide's 0.45 because a territory is
/// a boundary rather than a hint, and well under a selection ring's 1.0 because it is not
/// an object's outline — see `push_territory`.
const TERRITORY_EDGE_ALPHA: f32 = 0.60;
const TERRITORY_EDGE_ALPHA_LIVE: f32 = 0.90;
/// The edge of a region nobody has selected. Still clearly a boundary — this is the whole
/// point of drawing it at all — and well under the selected one, so picking an orchestrator
/// visibly answers *"which of these is yours"*.
const TERRITORY_EDGE_ALPHA_STANDING: f32 = 0.30;

/// Where the caret is, what is selected, and the string both index into.
///
/// Byte offsets into `text`, which is what `vellum_text::Layout::caret` and
/// `selection_boxes` index by.
///
/// **The string travels with the cursor rather than being read back off the item**, and
/// that is what lets a caret sit inside a table cell or on a mind-map node. Those parts'
/// words are inside an opaque JSON token, so a painter asked to re-derive "the text of slot
/// 7" would have to re-implement the slot-to-cell mapping that `crate::actions` already did
/// when the session began — a second copy of a mapping, in another module, that would go
/// wrong silently. It is also strictly more correct for the simple kinds: the offsets came
/// from this exact string, and every keystroke writes it through to the document, so
/// indexing the buffer's own bytes cannot disagree with itself even for one frame.
// No `Eq`: `idle_for` is an `f32`. `PartialEq` is what the comparisons here need, and
// deriving `Eq` over a float is a lie regardless.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TextCursor<'a> {
    pub scene: SceneId,
    pub slot: u16,
    /// Seconds since the caret last moved or the text last changed, for the blink.
    ///
    /// Supplied by the app because the painter has no clock of its own and must not grow
    /// one: a frame that reads `Instant::now()` is a frame whose output depends on when it
    /// ran, which is exactly what `--screenshot` cannot reproduce.
    pub idle_for: f32,
    pub cursor: usize,
    /// The other end of the selection; equal to `cursor` when nothing is selected.
    pub anchor: usize,
    /// The words being edited, exactly as the session's buffer holds them.
    pub text: &'a str,
}

/// A pen stroke mid-gesture, before it is an item.
///
/// The points are **absolute** world units, unlike [`vellum_doc::ItemKind::Ink`],
/// whose points are relative to the item's placement. The placement does not exist
/// yet — it is derived from the finished path's bounds — so there is nothing to be
/// relative to.
#[derive(Debug, Clone, Copy)]
pub struct LiveStroke<'a> {
    /// In the order they were sampled.
    pub points: &'a [WorldPoint],
    /// Already carries the pen kind's alpha; a highlighter arrives translucent.
    pub color: Rgba,
    /// Stroke width in world units.
    pub thickness: f64,
}

/// The item a create tool is sweeping out, as it will look when the button comes up.
///
/// The box comes from [`crate::actions::ActiveState::swept_placement`] — the same function
/// that commits the item — so the preview cannot be a rectangle away from the result.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Placing {
    /// Centre and extent in board units, exactly as the finished item's placement.
    pub placement: vellum_doc::Placement,
    pub look: PlacingLook,
}

/// How a [`Placing`] should be drawn.
///
/// The tool is resolved to a *look* by the app rather than matched on here, for the same
/// reason [`TextCursor`] carries its own string: this module draws the board and knows
/// nothing about the palette above it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PlacingLook {
    /// A frame: opaque white with its hairline edge, which is exactly what it becomes.
    Frame,
    /// A sticky, in the fill it will be created with.
    Sticky,
    /// The form the shape flyout has armed — drawn through the same SDF path the placed
    /// shape uses, so an ellipse previews as an ellipse rather than as its bounding box.
    Shape(Shape),
    /// The four Agent Canvas nodes: the surface card with its hairline, which is exactly
    /// what each of them is built on.
    ///
    /// Its own look rather than [`PlacingLook::Ghost`] because the answer *is* known here —
    /// an agent, a note, a file tree and a browser are all a bordered card before anything
    /// fills them — and feedback 23's rule is that a preview which disagrees with what it
    /// previews is worse than none, since it is believed. The corollary holds too: a
    /// preview that could tell the truth and does not is a smaller version of the same fault.
    Card,
    /// Everything whose final look is not known until it exists: a table, a chart, a mind
    /// map, a kanban, a text box. An accent ghost, like the marquee — it reports the box
    /// honestly and does not pretend to be a picture of the result.
    Ghost,
}

/// What a frame drew, for the HUD and for tests.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PaintStats {
    /// Items the R-tree returned for this viewport.
    pub visible: usize,
    /// Items actually emitted, after frame clipping.
    pub drawn: usize,
    /// Text blocks skipped for being too small to read.
    pub text_skipped: usize,
    /// Text blocks that wanted shaping this frame and did not get it, because
    /// [`TEXT_LAYOUT_BUDGET`] was already spent.
    ///
    /// **Distinct from `text_skipped`, and the number that matters.** A skipped block is
    /// *greeked* — it draws bars, so the board still reads as a board. A deferred one
    /// draws **nothing at all** (the `None` arm below), and until this reaches zero the
    /// board is missing words the user is waiting for. Counting it is what makes
    /// "frames-to-quiet after an edit" measurable rather than eyeballed: an edit
    /// invalidates every layout on the board, so this spikes and then drains over however
    /// many frames the ration takes.
    pub text_deferred: usize,
    /// Glyphs the atlas could not supply. Anything but zero is a bug in the
    /// prepare-then-draw ordering, and it shows on screen as missing characters.
    pub glyphs_missing: usize,
    /// Images referenced by a visible item that had no texture this frame.
    pub images_pending: usize,
    /// Tables laid out this frame. Each one shapes every cell, which is the most
    /// expensive thing a single item can ask for.
    pub tables: usize,
    /// Charts rebuilt this frame.
    pub charts: usize,
    /// Kanban boards laid out this frame. Like a table, each one shapes every card.
    pub kanbans: usize,
    /// Mind maps drawn this frame. A map is laid out at most once per frame however
    /// many nodes it has — see `CachedMindMap` — so this counts maps, not tidy passes.
    pub mindmaps: usize,
    pub draws: DrawStats,
}

/// Which open board a [`SceneId`]-keyed cache belongs to.
///
/// Handed out by [`crate::editor::Editor`], one per open board, from a process-wide
/// counter. It exists because neither of the two things that look like they identify a
/// board actually do: `SceneId`s restart at zero for every [`Projection`], and
/// generations restart at one. See [`Painter::sync`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BoardEpoch(pub u64);

/// The first slot a table's cells — or a mind map's node labels — occupy. Below it are
/// the two every kind has: the item's own body and its label.
pub(crate) const CELL_SLOT_BASE: u16 = 2;

/// A link card's **blurb**, which needs a block of its own.
///
/// Miro's card is three typographic voices — a muted site name, a large dark title, a smaller
/// grey description — and a block carries one colour and one size, so three voices is three
/// blocks. `PRIMARY` is the title and `SECONDARY` the provider row; this is the third.
///
/// It shares its number with `CELL_SLOT_BASE` rather than pushing that up, and the collision
/// is safe by construction: the only readers that treat a slot as an *index* are the table,
/// mind-map and kanban arms of [`Painter::block`], each gated on its own `ItemKind`, and a
/// card is none of them. Bumping `CELL_SLOT_BASE` instead would have renumbered every cell in
/// every table on every board — the kind of change that reads as harmless and moves the caret
/// one cell sideways.
pub(crate) const CARD_BLURB_SLOT: u16 = CELL_SLOT_BASE;

/// A table's laid-out grid, and what it was laid out against.
///
/// Held for a frame at a time so that a table is laid out **once** rather than once per
/// cell. Both text passes and the drawing all ask for the same grid, and a 3 x 3 table
/// re-laid per slot would shape its nine cells nine times.
#[derive(Debug)]
struct CachedTable {
    generation: u64,
    /// The box it was fitted to, since a resize changes every column.
    size: (f64, f64),
    layout: vellum_table::TableLayout,
    /// The decoded model. Kept beside the layout because `CellLayout` carries a cell's
    /// *geometry* and not its words — the text is still in the table, reached by the
    /// cell's anchor.
    table: vellum_table::Table,
}

/// A mind map's laid-out tree, and what it was laid out against.
///
/// Held for the same reason [`CachedTable`] is: the drawing and both text passes all ask
/// for the same layout, and laying a nine-node map out per slot would shape nine labels
/// nine times. Unlike a table, the layout does **not** depend on the item's box — node
/// boxes come from shaped text, so a tidy tree has one natural size — which is why the
/// box is not part of the key here and the fit scale is derived from it instead.
#[derive(Debug)]
struct CachedMindMap {
    generation: u64,
    layout: vellum_mindmap::Layout,
    /// The decoded model. Kept beside the layout because a `Placement` carries a node's
    /// *rectangle* and not its words or its colours; those are still on the tree.
    model: crate::mindmap::MindMapModel,
    /// The branches, in the model's connector form. Regenerated with the layout rather
    /// than per frame — they are a few hundred points and nothing between frames moves
    /// them.
    connectors: Vec<vellum_mindmap::ConnectorPath>,
    /// The map's natural extent, so the fit scale is one division rather than a walk.
    natural: (f64, f64),
}

impl CachedMindMap {
    /// How much the map has to shrink to sit inside the item's box.
    ///
    /// Uniform, and the smaller of the two ratios: a map stretched to a dragged box
    /// would put its text at one aspect and its branches at another. Resizing a mind
    /// map therefore scales it, which is what [`crate::mindmap`]'s honest-limits note
    /// records — a tidy tree's extent is determined by the tree, not chosen.
    fn scale(&self, size: (f64, f64)) -> f64 {
        crate::mindmap::fit_scale(self.natural, size)
    }
}

/// One run of text a kanban board draws, and where.
///
/// A kanban has three kinds of label — the board's title, a column's header, a card's —
/// at different sizes and colours, and the number of them depends on the data. So rather
/// than deriving a slot's meaning from arithmetic over columns and cards, the layout
/// flattens them into this list once and a slot is an index into it. The mind map's node
/// slots could be expressed the same way; the table's cannot, because a merged cell's
/// index is `vellum-table`'s to define.
///
/// [`kanban_runs`] is `pub(crate)` for one reason: it *is* the slot-to-field mapping, and
/// the press path needs the same one to decide what a double click landed on. Two copies of
/// this flattening — one that draws and one that decides — would be a caret that appears on
/// a card and types into its neighbour, and nothing would catch it but the eye.
#[derive(Debug, Clone)]
pub(crate) struct KanbanRun {
    /// Where the text goes, in the item's own space, already inset for padding.
    rect: vellum_flow::Rect,
    /// What is drawn — which for a column header is the title *plus* its count.
    text: String,
    /// Which field the run belongs to, so a click can be turned into an edit.
    part: crate::edit::EditPart,
    /// The field's own value, without any decoration [`Self::text`] adds.
    ///
    /// A column header draws `"To do  3/5"`, and the editable field behind it is `"To do"`.
    /// A caret seeded from the drawn string would put the count in the buffer and write it
    /// back into the title, so the two are kept apart: `text` is drawn, `field` is edited,
    /// and while a run *is* being edited the painter draws `field` — see
    /// `Painter::kanban_run_block`, where the count disappearing while you type the title is
    /// the visible consequence and the correct one.
    field: String,
    font_size: f64,
    /// Muted ink rather than primary — a column header is a label, not a heading.
    muted: bool,
}

impl KanbanRun {
    pub(crate) const fn part(&self) -> crate::edit::EditPart {
        self.part
    }

    pub(crate) fn field(&self) -> &str {
        &self.field
    }

    /// The run's box in the item's own space, for a hit test that wants the *label* rather
    /// than the card it sits on.
    pub(crate) const fn rect(&self) -> vellum_flow::Rect {
        self.rect
    }
}

/// A kanban board's laid-out columns, and what they were laid out against.
///
/// Keyed on the box as well as the generation, like [`CachedTable`] and unlike
/// [`CachedMindMap`]: a kanban's column width comes straight from the item's width, and
/// every card's measured height comes from that width, so a resize changes everything.
#[derive(Debug)]
struct CachedKanban {
    generation: u64,
    size: (f64, f64),
    layout: vellum_flow::KanbanLayout,
    /// Every label, flattened, so a text slot is an index. The measured board itself is
    /// **not** kept: `runs` already holds every string the drawing needs, and holding a
    /// second copy of the whole board per visible kanban would be state nothing reads.
    /// The press path decodes the token afresh — see `ActiveState::card_under` for why
    /// that is right rather than merely acceptable.
    runs: Vec<KanbanRun>,
}

/// What a cached shape silhouette belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct ShapeMeshKey {
    id: SceneId,
    band: i32,
}

/// One ink stroke's triangles, and the zoom band they were tessellated for.
#[derive(Debug)]
struct CachedInk {
    generation: u64,
    band: i32,
    mesh: vellum_ink::Mesh,
}

/// Owns everything that survives between frames: layouts, tessellated ink, and the
/// scratch buffers a steady-state frame must not reallocate.
pub struct Painter {
    text: TextCache,
    ink: HashMap<SceneId, CachedInk>,
    /// Laid-out tables, one frame at a time. See [`CachedTable`].
    tables: HashMap<SceneId, CachedTable>,
    /// Laid-out mind maps, one frame at a time. See [`CachedMindMap`].
    mindmaps: HashMap<SceneId, CachedMindMap>,
    /// Laid-out kanban boards, one frame at a time. See [`CachedKanban`].
    kanbans: HashMap<SceneId, CachedKanban>,
    /// The Agent Canvas nodes' pieces — transcript runs, tree rows, option cards.
    ///
    /// Unlike every other cache here this is keyed on a **frame counter** as well as on the
    /// projection, because what an agent node draws is not in the document: see
    /// [`CachedNode::frame`].
    nodes: HashMap<SceneId, CachedNode>,
    /// Bumped once per [`Painter::paint`]. See [`CachedNode::frame`].
    frame: u64,
    /// Tessellated silhouettes for the shapes an SDF cannot express.
    ///
    /// Keyed on the LOD band as well as the item, like the ink cache: the mesh's
    /// flattening tolerance is chosen from the drawn size, so a zoom changes what it
    /// should be. Unlike the ink cache it is *not* keyed on the projection generation
    /// — the mesh is in unit-box space and the transform carries the size, so a resize
    /// moves the transform and leaves the mesh correct.
    shapes: HashMap<ShapeMeshKey, vellum_shapes::ShapeMesh>,
    router: Router,
    order: Vec<(i32, SceneId)>,
    glyphs: Vec<(GlyphKey, GlyphImage)>,
    /// Time spent shaping new text this frame, against [`TEXT_LAYOUT_BUDGET`].
    text_spent: Duration,
    /// Blocks that wanted shaping this frame and were refused it by the budget.
    ///
    /// Counted **here** rather than at the `None` arm in `paint`, because that arm is
    /// reached for several other reasons — a zero-height box, a slot with no words — and a
    /// number that conflates them would report a board as never settling when it had.
    /// Reset with `text_spent`, which is the same lifetime.
    text_deferred: usize,
    /// The board the caches currently hold entries for. See [`Painter::sync`].
    painted_board: BoardEpoch,
    /// The projection generation the caches were last pruned against.
    pruned_generation: u64,
    /// The scale glyph bitmaps were last rasterised at. See [`Painter::prepare_text`].
    last_scale: f32,
    /// Where the block being edited was drawn, in physical pixels, as of the last frame.
    ///
    /// Recorded rather than recomputed because a click has to resolve against **what the
    /// user saw**, and what they saw is the last painted frame. Recomputing would mean
    /// rebuilding a `DrawContext` outside the paint loop, and would answer a question
    /// about a frame that has not been drawn yet.
    ///
    /// At most one frame stale, and a frame is requested unconditionally from
    /// `about_to_wait`, so a camera move is always followed by a repaint before the next
    /// click can arrive.
    /// Where the edited block was drawn last frame, and the scale it was drawn at.
    ///
    /// The scale rides along because a click has to undo exactly the transform the
    /// paint applied, and for a frame title — which is caret-editable and floors its
    /// magnification — that is not the camera's zoom. See [`Block::scale`].
    edited_origin: Option<(BlockKey, ScreenPoint, f32)>,
}

impl std::fmt::Debug for Painter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Painter")
            .field("text", &self.text)
            .field("ink", &self.ink.len())
            .finish()
    }
}

impl Painter {
    pub fn new(text: TextCache) -> Self {
        Self {
            text,
            ink: HashMap::new(),
            shapes: HashMap::new(),
            tables: HashMap::new(),
            mindmaps: HashMap::new(),
            kanbans: HashMap::new(),
            nodes: HashMap::new(),
            frame: 0,
            router: Router::default(),
            order: Vec::new(),
            glyphs: Vec::new(),
            text_spent: Duration::ZERO,
            text_deferred: 0,
            // No board has been painted yet. `Editor` counts epochs up from zero, so
            // this cannot collide with a real one, and the first `sync` clears caches
            // that are already empty.
            painted_board: BoardEpoch(u64::MAX),
            pruned_generation: 0,
            // Not a real zoom, so the first frame evicts an empty cache and records
            // the scale it actually rasterised at.
            last_scale: f32::NAN,
            edited_origin: None,
        }
    }

    pub fn text_mut(&mut self) -> &mut TextCache {
        &mut self.text
    }

    /// Where the block being edited was drawn last frame, in physical pixels.
    ///
    /// The one thing a click on a caret needs and cannot work out for itself. `None`
    /// before the block has been painted with a caret in it, or once the caret has moved
    /// to another slot — both of which mean "there is nothing on screen to click into".
    pub fn edited_origin(&self, key: BlockKey) -> Option<(ScreenPoint, f32)> {
        self.edited_origin
            .filter(|(painted, ..)| *painted == key)
            .map(|(_, origin, scale)| (origin, scale))
    }

    /// Cached text layouts. For the flight recorder — a count that climbs while the
    /// board does not is a cache that has stopped being pruned.
    pub fn text_layouts(&self) -> usize {
        self.text.len()
    }

    /// Tessellated ink strokes held between frames.
    pub fn ink_meshes(&self) -> usize {
        self.ink.len()
    }

    /// Rasterised glyph bitmaps held by the text engine.
    pub fn glyph_bitmaps(&self) -> usize {
        self.text.glyph_bitmaps()
    }

    /// Drops everything cached for items the board no longer holds.
    ///
    /// Driven by the projection's generation rather than by a flag the caller has to
    /// remember to set: a rebuild is the only thing that can retire an item, and it
    /// always bumps the counter. Call once per frame — it costs one comparison until
    /// something actually changed.
    ///
    /// # Why the board's identity is a separate argument
    ///
    /// These caches are keyed on [`SceneId`], and **a `SceneId` only means anything
    /// within one board**: every [`Projection`] interns from zero
    /// (`Projection::intern`) and every freshly-opened board sits at generation 1,
    /// because `Editor::in_memory` reprojects exactly once. So two untouched boards
    /// collide on *both* halves of the old key, and switching between them returned
    /// the first board's laid-out text — `TextCache::layout` sees a matching
    /// generation and hands back the entry — for the second board's items. Same for
    /// tessellated ink.
    ///
    /// A switch therefore **clears** rather than retains. Retaining cannot work here:
    /// `projection.get(id)` answers for the *incoming* board, so the outgoing board's
    /// entries at the same ids look alive and survive the prune. Clearing is also what
    /// releases the parked board's layouts and meshes, which is the point — nothing
    /// else ever freed them.
    pub fn sync(&mut self, board: BoardEpoch, projection: &Projection) {
        if self.painted_board != board {
            self.painted_board = board;
            self.pruned_generation = projection.generation();
            self.text.clear();
            self.ink.clear();
            // The structured widgets' layouts hang off a `SceneId` exactly as the ink
            // meshes do, and were being neither cleared here nor pruned below — so a
            // parked board's tables and mind maps stayed resident, and a deleted one's
            // never came back. Found while adding the third such cache; the entries are
            // small, but "small and unbounded" is how the texture leak started too.
            self.tables.clear();
            self.mindmaps.clear();
            self.kanbans.clear();
            self.nodes.clear();
            self.shapes.clear();
            return;
        }
        if self.pruned_generation == projection.generation() {
            return;
        }
        self.pruned_generation = projection.generation();
        self.text.retain(|id| projection.get(id).is_some());
        self.ink.retain(|id, _| projection.get(*id).is_some());
        self.tables.retain(|id, _| projection.get(*id).is_some());
        self.mindmaps.retain(|id, _| projection.get(*id).is_some());
        self.kanbans.retain(|id, _| projection.get(*id).is_some());
        self.nodes.retain(|id, _| projection.get(*id).is_some());
        self.shapes.retain(|key, _| projection.get(key.id).is_some());
    }

    /// Builds the frame's draw list.
    ///
    /// The renderer is borrowed mutably because two of its caches have to be filled
    /// *before* the list that references them is built: the glyph atlas, and the
    /// texture manager by way of [`Assets`].
    pub fn paint(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        renderer: &mut Renderer,
        assets: &mut Assets,
        list: &mut DrawList,
        ctx: &DrawContext<'_>,
    ) -> PaintStats {
        let mut stats = PaintStats::default();
        list.clear();
        self.text_spent = Duration::ZERO;
        self.text_deferred = 0;
        // An Agent Canvas node's contents are not in the document, so nothing the projection
        // knows about moves when an agent speaks. This counter is what retires last frame's
        // layout of one — see [`CachedNode::frame`]. Wrapping, because a counter that
        // saturated would silently stop invalidating after 2^64 frames rather than loudly.
        self.frame = self.frame.wrapping_add(1);

        // Images the decode workers finished, onto the GPU — **before** the list that
        // references them is built, so one that arrived since the last frame is drawn
        // this frame rather than next. This is the only place that holds the device, the
        // queue, the texture manager and `Assets` at once, which is why it lives here
        // rather than in `app`'s frame loop.
        assets.collect(device, queue, renderer.textures_mut());
        assets.enforce_budget(renderer.textures_mut());

        let camera = ctx.camera;
        let board = list.view(View::board(camera));
        let screen = list.view(View::screen(camera.viewport()));

        self.order.clear();
        self.order.extend(
            ctx.projection
                .scene()
                .query_viewport(camera)
                .map(|item| (item.z, item.id)),
        );
        // Back to front, ties by id, matching `Scene::hit_test` so what the user
        // clicks is what they can see. Frames sort behind everything else because
        // `crate::project` gives them a negative `z`, not because of anything here —
        // doing it here instead would paint a frame behind a sticky while leaving the
        // hit-test picking the frame, and the click would land on the thing underneath.
        self.order.sort_unstable();
        stats.visible = self.order.len();

        // Before any item: the grid is the field things sit *on*, so drawing it after
        // would put dots on top of the stickies.
        if ctx.pattern != Pattern::Plain {
            push_grid(list, ctx, screen);
        }

        // Pass one: make sure every glyph this frame needs is in the atlas. It has
        // to finish before a single glyph is pushed, because `push_layout` looks
        // slots up rather than requesting them.
        self.prepare_text(device, queue, renderer.atlas_mut(), ctx, &mut stats);

        // The orchestrator's region, **behind everything** — `docs/07-agent-canvas.md` §9.
        // After the grid and before the items: a territory is a wash *on* the board's
        // surface rather than part of it, and a tint under the grid would put dots on top of
        // the one thing that says which part of the board an agent owns.
        //
        // After `prepare_text`, because the label goes through the same atlas every other
        // glyph does and `push_layout` looks slots up rather than requesting them.
        self.push_territory(list, ctx, renderer.atlas(), screen);

        // Pass two: geometry and glyphs, strictly in paint order.
        for index in 0..self.order.len() {
            let (_, id) = self.order[index];
            let Some(projected) = ctx.projection.get(id) else { continue };
            if clipped_by_frame(projected, ctx.projection) {
                continue;
            }
            stats.drawn += 1;
            self.push_item(
                device,
                queue,
                renderer,
                assets,
                list,
                ctx,
                id,
                projected,
                (board, screen),
                &mut stats,
            );
        }

        push_stroke(list, ctx, board);
        push_placing(list, ctx, board);
        push_pending_connector(list, ctx, board);
        self.push_selection(list, ctx, board);
        // After the ring and the handles, so a port sitting just outside a corner is drawn
        // over the handle rather than under it — which matches the press path, where a port
        // is asked about first.
        push_ports(list, ctx, board);
        push_card_drop(list, ctx, board);
        // After the selection ring, so a guide running along an item's edge is not hidden
        // under the ring that is on the same edge; before the marquee, which is a different
        // gesture and cannot be up at the same time.
        push_guides(list, ctx, screen);
        push_marquee(list, ctx, screen);
        push_minimap(list, ctx, screen);

        stats.draws = list.stats();
        // Accumulated inside `block`, which has no `stats` to reach — see the field.
        stats.text_deferred = self.text_deferred;
        stats
    }

    /// Rasterises whatever the atlas is missing for this frame's text.
    fn prepare_text(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        atlas: &mut GlyphAtlas,
        ctx: &DrawContext<'_>,
        stats: &mut PaintStats,
    ) {
        self.glyphs.clear();
        let scale = ctx.camera.zoom() as f32;

        // A glyph bitmap is keyed by its *device* size, so every bitmap cached at a
        // different scale is unreachable the moment the zoom moves — no future key can
        // match one. Neither of the engine's bitmap caches evicts on its own, so
        // without this a zoom gesture leaves a fresh set resident per frame for the
        // life of the process. Dropping them costs nothing: during a gesture every key
        // is new anyway, so the rasterising below already runs regardless, and while
        // the zoom is still this never fires and the cache does its job.
        if scale != self.last_scale {
            self.text.forget_glyph_bitmaps();
            self.last_scale = scale;
        }

        for index in 0..self.order.len() {
            let (_, id) = self.order[index];
            let Some(projected) = ctx.projection.get(id) else { continue };
            if clipped_by_frame(projected, ctx.projection) {
                continue;
            }
            for slot in 0..self.slots_of(id, projected, projected.generation, ctx) {
                let block = match self.block(id, projected, slot, ctx) {
                    Some(Painted::Glyphs(block)) => block,
                    // A greeked block rasterises nothing — that is the point of it.
                    Some(Painted::Greeked(_)) => {
                        stats.text_skipped += 1;
                        continue;
                    }
                    None => continue,
                };
                let origin = (block.origin.x as f32, block.origin.y as f32);
                let key = BlockKey::new(id, slot);
                // Asking the engine for entries is a rasterise; only do it when the
                // atlas is actually short of something, which after the first frame
                // at a given zoom is never.
                let missing = self.text.layout_of(key).is_some_and(|layout| {
                    layout.glyphs().any(|glyph| {
                        let key = glyph.physical(origin, scale).key;
                        atlas.slot(key).is_none() && !atlas.is_blank(key)
                    })
                });
                if missing {
                    self.text.rasterise_into(key, origin, scale, &mut self.glyphs);
                }
            }
        }

        // The territory labels. **Not items**, so they are not in `self.order` and cannot be
        // reached by the loop above — and a region can be on screen while the node that owns
        // it is culled, which is precisely when a label saying whose region this is earns its
        // place. That case stopped being an edge one when the regions became always-on:
        // several orchestrators' rectangles overlapping is exactly when the chips are the
        // only way to tell them apart. Rasterised at scale 1.0 for the reason
        // [`Painter::territory_label`] gives.
        //
        // Cloned rather than borrowed: `territory_label` needs `&mut self` for the text
        // cache, and `ctx.territories` would otherwise be borrowed across it. Empty on every
        // board with no orchestrator, so the allocation is not one an ordinary board makes.
        if !ctx.territories.is_empty() {
            for tint in ctx.territories.clone() {
                let Some(label) = self.territory_label(ctx, &tint) else { continue };
                let missing = self.text.layout_of(label.key).is_some_and(|layout| {
                    layout.glyphs().any(|glyph| {
                        let key = glyph.physical(label.origin, 1.0).key;
                        atlas.slot(key).is_none() && !atlas.is_blank(key)
                    })
                });
                if missing {
                    self.text.rasterise_into(label.key, label.origin, 1.0, &mut self.glyphs);
                }
            }
        }

        if !self.glyphs.is_empty()
            && let Err(error) = atlas.prepare(device, queue, &self.glyphs)
        {
            // Not fatal: the frame draws with whatever is resident, which is text
            // with holes in it rather than no frame at all.
            log::warn!("glyph atlas: {error}");
        }
    }

    /// Lays out the territory's label and works out where on screen it goes.
    ///
    /// # Why this is not an item's block
    ///
    /// [`Painter::block`] positions text from `projected.rect()` and an [`Anchor`], which is
    /// the right machinery for words that belong *inside* a box. A territory's label belongs
    /// to a rectangle that is not an item, is very often larger than the screen, and has to
    /// stay the same size at every zoom — none of which `block` can express. Teaching it a
    /// case with no `Projected` behind it would put a special arm in the one function every
    /// kind on the board goes through.
    ///
    /// # Laid out in **device** pixels, and pushed at scale 1.0
    ///
    /// Everything else here is shaped in world units and multiplied by the zoom at push
    /// time, which is right for text that is part of the board. This is chrome: it must be
    /// the same 12 px at a fitted 4% and at 8×, the way a guide's dash cadence and the
    /// selection ring's weight already are. Shaping at the device size and pushing with
    /// `scale = 1.0` is how that is spelled — the alternative, dividing a world size by the
    /// zoom, reshapes the label on every frame of a zoom gesture and fills the atlas.
    ///
    /// # The label is clamped into the region *and* into the viewport
    ///
    /// A region is usually bigger than the window and its top-left corner is usually off
    /// screen, so a label pinned to that corner is a label nobody ever sees — which is the
    /// whole of what it was for. It slides along the region's own edges to stay in view, and
    /// never leaves the region, so it cannot come to sit over a neighbouring orchestrator's.
    ///
    /// # It takes the tint rather than reading it off the context
    ///
    /// There are several regions on screen now, not one, so *"the territory's label"* is no
    /// longer a question the context can answer on its own. Passing the tint is also what
    /// keeps the two callers — the glyph-preparation pass and the paint — asking about the
    /// **same** region in the same order, which is the `card_layout` rule: a second copy of a
    /// layout is a label rasterised for one rectangle and drawn against another.
    fn territory_label(
        &mut self,
        ctx: &DrawContext<'_>,
        tint: &TerritoryTint,
    ) -> Option<TerritoryLabel> {
        if tint.label.trim().is_empty() {
            return None;
        }
        let key = BlockKey::new(tint.scene, TERRITORY_LABEL_SLOT);
        let style = vellum_doc::Style {
            font_size: Some(f64::from(TERRITORY_LABEL_SIZE)),
            ..vellum_doc::Style::default()
        };
        // The node's own projection generation, which is exactly what moves when its role
        // label changes and exactly what does *not* move while the board is panned or a
        // rectangle is being swept. The `pending` flag deliberately does not enter here: the
        // words are the same either way, and folding it in would reshape the label twice per
        // gesture to produce the identical layout.
        let words = tint.label.clone();
        let (layout, _) = self.text.layout(key, tint.generation, &style, None, || {
            vellum_text::StyledText::plain(words)
        });
        let extent = (layout.extent.width, layout.extent.height);

        let (min, max) = tint.screen_rect(ctx.camera);
        let viewport = ctx.camera.viewport();
        let plate = (
            extent.0 + TERRITORY_LABEL_PAD * 2.0,
            extent.1 + TERRITORY_LABEL_PAD,
        );
        // Inside the region's corner, then slid to stay on screen — but never past the
        // region's far edge, so a label that cannot fit inside its own region sits at the
        // corner rather than floating somewhere it does not describe.
        let slide = |near: f32, far: f32, size: f32, limit: f32| {
            let inset = near + TERRITORY_LABEL_INSET;
            inset.max(0.0).min((far - size - TERRITORY_LABEL_INSET).max(inset)).min(
                (limit - size).max(0.0),
            )
        };
        let x = slide(min.0, max.0, plate.0, viewport.width as f32);
        let y = slide(min.1, max.1, plate.1, viewport.height as f32);

        Some(TerritoryLabel {
            key,
            // The glyphs sit inside the plate, and `Layout`'s own origin is the block's
            // top-left, which is why the pad is added rather than subtracted.
            origin: (x + TERRITORY_LABEL_PAD, y + TERRITORY_LABEL_PAD / 2.0),
            plate: [x, y, plate.0, plate.1],
        })
    }

    /// The orchestrator's region: a wash, a dashed edge and a label saying whose it is.
    ///
    /// **Screen view throughout, and that is the decision.** A region is a rectangle in
    /// world units and the fill could as easily be pushed in the board view — but the edge
    /// and the label cannot: a world-unit dash is a solid line when zoomed out and three
    /// dashes across the window when zoomed in (`push_dashed`, and `push_grid` before it),
    /// and a world-sized label is illegible at a fitted 4% and a banner at 8×. Drawing all
    /// three in one view means one batch and one arithmetic, rather than a fill that agrees
    /// with an edge only when nothing has been rounded.
    ///
    /// A region is axis-aligned and never rotated, so converting its two corners to screen
    /// is exact — the conversion that would *not* be is the one this deliberately avoids by
    /// carrying a centre and an extent all the way from `vellum_agent::Territory`.
    ///
    /// **Dashed rather than solid**, for feedback 25a's reason applied to a second kind of
    /// chrome: a solid accent hairline is what a selection ring and a shape's border are, so
    /// an edge drawn that way reads as belonging to an object. Nothing else on this canvas
    /// is dashed except a connector that was asked to be, and a territory is the one thing
    /// on the board that is not an object at all.
    fn push_territory(
        &mut self,
        list: &mut DrawList,
        ctx: &DrawContext<'_>,
        atlas: &GlyphAtlas,
        screen: u32,
    ) {
        // Sorted so the loudest is drawn last and therefore on top. Without it a standing
        // region that happens to come later in the projection washes over the one being
        // swept, and the whole three-rung emphasis says nothing.
        let mut order: Vec<&TerritoryTint> = ctx.territories.iter().collect();
        order.sort_by_key(|tint| match tint.emphasis {
            TerritoryEmphasis::Standing => 0,
            TerritoryEmphasis::Selected => 1,
            TerritoryEmphasis::Sweeping => 2,
        });
        for tint in order {
            self.push_one_territory(list, ctx, atlas, screen, tint);
        }
    }

    /// One region. Split out of [`Self::push_territory`] so the loop above holds the
    /// ordering decision and this holds the drawing, rather than one function holding both.
    fn push_one_territory(
        &mut self,
        list: &mut DrawList,
        ctx: &DrawContext<'_>,
        atlas: &GlyphAtlas,
        screen: u32,
        tint: &TerritoryTint,
    ) {
        let (min, max) = tint.screen_rect(ctx.camera);
        let viewport = ctx.camera.viewport();
        let (view_w, view_h) = (viewport.width as f32, viewport.height as f32);

        // Entirely off screen: nothing to draw, and — the part that matters — nothing to
        // *dash*, since an edge a long way outside the window would otherwise emit a run of
        // quads nobody can see. The same clip `push_dashed` applies along its own axis,
        // applied here across both. This became load-bearing rather than tidy when every
        // region started drawing at once: a board of eight orchestrators is eight of these.
        if max.0 <= 0.0 || max.1 <= 0.0 || min.0 >= view_w || min.1 >= view_h {
            return;
        }

        list.use_view(screen);
        let theme = ctx.theme;
        let (fill_alpha, edge_alpha) = tint.emphasis.alphas();

        // The wash, clipped to the window. A region is routinely hundreds of screens wide
        // and a quad that big is a quad the rasteriser has to clip anyway; doing it here
        // keeps the numbers finite, which is what stops a zoomed-in board handing the GPU
        // coordinates it cannot represent.
        let clipped = [
            min.0.max(0.0),
            min.1.max(0.0),
            (max.0.min(view_w) - min.0.max(0.0)).max(0.0),
            (max.1.min(view_h) - min.1.max(0.0)).max(0.0),
        ];
        if clipped[2] > 0.0 && clipped[3] > 0.0 {
            list.push_quad(QuadInstance::solid(
                [clipped[0], clipped[1]],
                [clipped[2], clipped[3]],
                theme.accent.with_alpha(fill_alpha),
            ));
        }

        // The four edges, each only when it is actually in the window. A hairline, and a
        // hair over one pixel so it survives the rounding either way — the guides' rule.
        let colour = theme.accent.with_alpha(edge_alpha);
        let weight = HAIRLINE.max(1.0);
        for (across, axis) in [
            (min.0, crate::snap::Axis::Vertical),
            (max.0, crate::snap::Axis::Vertical),
            (min.1, crate::snap::Axis::Horizontal),
            (max.1, crate::snap::Axis::Horizontal),
        ] {
            let limit = if axis == crate::snap::Axis::Vertical { view_w } else { view_h };
            if across < 0.0 || across > limit {
                continue;
            }
            let (a, b) = match axis {
                crate::snap::Axis::Vertical => (
                    ScreenPoint::new(f64::from(across), f64::from(min.1)),
                    ScreenPoint::new(f64::from(across), f64::from(max.1)),
                ),
                crate::snap::Axis::Horizontal => (
                    ScreenPoint::new(f64::from(min.0), f64::from(across)),
                    ScreenPoint::new(f64::from(max.0), f64::from(across)),
                ),
            };
            push_dashed(list, ctx, axis, a, b, colour, weight);
        }

        // The label. A pale plate with an accent hairline rather than accent-on-accent:
        // charcoal on the teal is 5.3:1 and white on it is 3.2:1, so a filled accent plate
        // would have to carry `on_accent` — which this canvas palette does not have, and
        // inventing one here would be a fifth copy of a colour three files already share.
        let Some(label) = self.territory_label(ctx, tint) else { return };
        list.use_view(screen);
        list.push_quad(
            QuadInstance::solid(
                [label.plate[0], label.plate[1]],
                [label.plate[2], label.plate[3]],
                theme.surface,
            )
            .with_corner_radius(TERRITORY_LABEL_RADIUS)
            .with_border(colour, weight),
        );
        if let Some(layout) = self.text.layout_of(label.key) {
            list.push_layout(atlas, layout, [label.origin.0, label.origin.1], 1.0, theme.text);
        }
    }

    /// Emits one item.
    #[allow(clippy::too_many_arguments)]
    fn push_item(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        renderer: &mut Renderer,
        assets: &mut Assets,
        list: &mut DrawList,
        ctx: &DrawContext<'_>,
        id: SceneId,
        projected: &Projected,
        views: (u32, u32),
        stats: &mut PaintStats,
    ) {
        let (board, screen) = views;
        let camera = ctx.camera;
        let theme = ctx.theme;
        let (origin, (width, height)) = projected.rect();
        let position = camera.to_camera_relative(origin);
        let size = [width as f32, height as f32];
        let rotation = projected.rotation();
        let opacity = projected.opacity();

        list.use_view(board);
        match &projected.item.kind {
            ItemKind::Sticky { background, .. } => {
                let fill = background.map_or(theme.sticky, theme::convert);
                list.push_quad(
                    QuadInstance::solid(position, size, fill)
                        .with_corner_radius(STICKY_RADIUS)
                        .with_rotation(rotation)
                        .with_opacity(opacity),
                );
            }

            ItemKind::Text { .. } => {}

            ItemKind::Frame { .. } => {
                // A frame is opaque white unless it was given a colour of its own.
                // The board behind it now carries a grid by default, and a frame that
                // took the surface tint let the grid read straight through it — so the
                // thing a frame exists to do, mark off a region as its own, stopped
                // working the moment the grid arrived. White is the contrast.
                let fill = projected
                    .item
                    .style
                    .fill
                    .map_or(theme.frame_fill, theme::convert);
                // A frame's edge is chrome, so like the selection ring its world width
                // has to shrink as the board is zoomed in. `HAIRLINE` is one device
                // pixel at 100% — used raw it is invisible at a fitted zoom, which is
                // how a whole board is normally looked at, and a slab at 8×.
                let hairline = (f64::from(HAIRLINE) / camera.zoom()) as f32;
                list.push_quad(
                    QuadInstance::solid(position, size, fill)
                        .with_border(theme.border, hairline)
                        .with_rotation(rotation)
                        .with_opacity(opacity),
                );
            }

            ItemKind::Shape { form, .. } => {
                // Analytic wherever the shape allows it, which `vellum-shapes` says is
                // everything polygonal — a star included, because `sdf_params` falls
                // back to the outline's own vertices. The pipeline, the polygon arena
                // and the instance were already live on this board through the cards
                // above; this arm supplies a shape and a style and nothing else.
                let shape = crate::shapes::decode(form);
                let fill = projected.item.style.fill.map_or(theme.surface, theme::convert);
                let border = projected.item.style.stroke.map_or(theme.border, theme::convert);
                // A stroke width is in world units and the SDF border is drawn inside
                // the edge, so it needs no zoom compensation — unlike the frame's
                // hairline, which is chrome rather than part of the drawing.
                let border_width = projected.item.style.stroke_width.unwrap_or(f64::from(HAIRLINE));

                let size = Size::new(size[0], size[1]);
                if let Some(params) = shape.sdf_params(size) {
                    list.push_shape(
                        &params,
                        [position[0] + size.width / 2.0, position[1] + size.height / 2.0],
                        &ShapeStyle {
                            fill,
                            border,
                            border_width: border_width as f32,
                            rotation,
                            opacity,
                        },
                    );
                } else {
                    // The handful whose parameters do not survive a non-uniform scale
                    // — cloud, heart, cylinder, speech bubble, the document wave, arcs
                    // and wedges. Tessellated rather than skipped.
                    let band = lod_band(camera.zoom());
                    let key = ShapeMeshKey { id, band };
                    let mesh = self.shapes.entry(key).or_insert_with(|| {
                        let aspect = size.aspect();
                        shape
                            .tessellate(
                                aspect,
                                ShapeTessellation::for_size(size.width.max(size.height)),
                            )
                            .unwrap_or_else(|error| {
                                log::warn!("shape tessellation failed: {error}");
                                vellum_shapes::ShapeMesh::default()
                            })
                    });
                    let transform = list.meshes_mut().push_transform(MeshTransform::unit_box(
                        position,
                        [size.width, size.height],
                        rotation,
                    ));
                    let start = list.meshes().indices().len() as u32;
                    list.meshes_mut().push_shape_fill(
                        &mesh.fill,
                        fill.with_alpha(fill.a * opacity),
                        transform,
                    );
                    // The stroke mesh carries a normal per vertex and is widened here,
                    // so a border width is a uniform rather than a re-tessellation.
                    // The width is in unit-box space, hence the division.
                    let unit_width = border_width as f32 / size.width.max(size.height).max(1.0);
                    list.meshes_mut().push_shape_stroke(
                        &mesh.stroke,
                        unit_width,
                        border.with_alpha(border.a * opacity),
                        transform,
                    );
                    let end = list.meshes().indices().len() as u32;
                    list.push_meshes(start..end);
                }
            }

            ItemKind::Table { .. } => {
                // The same grid the text pass already built and shaped against, not a
                // second layout: one table, one layout, per frame. `Fit::Width` spreads
                // the item's width across the columns, and the row heights come from
                // real shaping — which is why this needs `crate::table`'s measurer and
                // not the stand-in `vellum-table` ships for headless use.
                //
                // Copied out rather than borrowed across the pushes below: the layout
                // lives in `self.tables` and `list` needs `self` free.
                let (ground, cells, borders) = {
                    let laid = &self
                        .table_layout(id, projected, projected.generation)
                        .layout;
                    let cells: Vec<(vellum_table::Rect, Option<vellum_table::Rgba>)> =
                        laid.cells.iter().map(|c| (c.rect, c.style.fill)).collect();
                    // Borders are drawn **on** a boundary rather than between two
                    // cells; `vellum-table` has already coalesced them, so an interior
                    // edge is one segment claimed by one side rather than two
                    // overlapping hairlines.
                    let borders: Vec<(f64, f64, f64, f64, Option<vellum_table::Rgba>)> = laid
                        .borders
                        .iter()
                        .map(|b| {
                            let (x, y) =
                                (b.from.x.min(b.to.x), b.from.y.min(b.to.y));
                            let thickness =
                                b.side.width.max(f64::from(HAIRLINE) / camera.zoom());
                            let (w, h) = match b.orientation {
                                vellum_table::Orientation::Vertical => (thickness, b.length()),
                                vellum_table::Orientation::Horizontal => (b.length(), thickness),
                            };
                            (x, y, w, h, b.side.color)
                        })
                        .collect();
                    (laid.fill, cells, borders)
                };

                // Camera-relative already, as above.
                let at = |x: f64, y: f64| [position[0] + x as f32, position[1] + y as f32];

                // The table's own ground, behind every cell.
                if let Some(fill) = ground {
                    list.push_quad(
                        QuadInstance::solid(position, size, table_colour(fill))
                            .with_rotation(rotation)
                            .with_opacity(opacity),
                    );
                }
                for (rect, fill) in &cells {
                    let Some(fill) = fill else { continue };
                    list.push_quad(
                        QuadInstance::solid(
                            at(rect.origin.x, rect.origin.y),
                            [rect.size.width as f32, rect.size.height as f32],
                            table_colour(*fill),
                        )
                        .with_rotation(rotation)
                        .with_opacity(opacity),
                    );
                }
                for (x, y, w, h, colour) in &borders {
                    let colour = colour.map_or(theme.border, table_colour);
                    list.push_quad(
                        QuadInstance::solid(at(*x, *y), [*w as f32, *h as f32], colour)
                            .with_rotation(rotation)
                            .with_opacity(opacity),
                    );
                }

                stats.tables += 1;
            }

            ItemKind::Chart { spec } => {
                // Rebuilt per frame rather than cached. A chart's geometry is a few
                // hundred marks at most and the layout is pure arithmetic over already
                // shaped labels — unlike a table, whose every cell is a shaping call.
                let chart = crate::chart::decode(spec);
                let geometry = crate::chart::build(
                    &chart,
                    self.text.engine_mut(),
                    (f64::from(size[0]), f64::from(size[1])),
                );

                // `position` is **already camera-relative** — see where it is computed
                // at the top of this method — so a chart-local offset is a plain
                // addition. Converting again subtracts the camera's centre twice, which
                // put every bar off-screen while the tessellated marks, which reach the
                // GPU through a transform rather than a quad, drew correctly.
                let at = |x: f32, y: f32| [position[0] + x, position[1] + y];
                let tint = |c: vellum_chart::Colour| {
                    Rgba::from_rgb8(c.r, c.g, c.b).with_alpha(f32::from(c.a) / 255.0 * opacity)
                };

                // The chart's own ground. Every gap and ring between marks is this
                // showing through rather than paint, so it has to be drawn first.
                list.push_quad(
                    QuadInstance::solid(position, size, tint(geometry.surface))
                        .with_rotation(rotation),
                );

                // Axes and the zero rule, under the marks: a bar sitting on the
                // baseline should cover it, not be cut by it.
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
                                // A full-radius corner on a square is a circle, which
                                // saves a second pipeline for a scatter point.
                                .with_corner_radius(dot.radius)
                                .with_rotation(rotation),
                            );
                        }
                        vellum_chart::Mark::Line(line) => push_local_mesh(
                            list,
                            position,
                            &crate::chart::polyline_mesh(&line.path, line.width),
                            tint(line.colour),
                            rotation,
                        ),
                        vellum_chart::Mark::Area(area) => push_local_mesh(
                            list,
                            position,
                            &crate::chart::ring_mesh(&area.outline),
                            tint(area.fill),
                            rotation,
                        ),
                        vellum_chart::Mark::Slice(slice) => push_local_mesh(
                            list,
                            position,
                            &crate::chart::slice_mesh(&slice.arc),
                            tint(slice.colour),
                            rotation,
                        ),
                    }
                }

                stats.charts += 1;
            }

            ItemKind::MindMap { .. } => {
                // The same tree the text pass laid out and shaped against, not a second
                // layout: one map, one tidy pass, per frame.
                //
                // Copied out rather than borrowed across the pushes below, as the table
                // arm does: the layout lives in `self.mindmaps` and `list` needs `self`
                // free.
                let (scale, nodes, branches) = {
                    let cached = self.mindmap_layout(id, projected, projected.generation);
                    let scale = cached.scale((f64::from(size[0]), f64::from(size[1])));
                    let nodes: Vec<_> = cached
                        .layout
                        .placements()
                        .iter()
                        .filter_map(|p| cached.model.map.get(p.node).map(|n| (p.rect, n.style)))
                        .collect();
                    // A branch's colour and width live on the **child**, which is what
                    // `vellum-mindmap`'s connector module says: a link belongs to the
                    // branch it feeds, not to the parent it leaves.
                    let branches: Vec<_> = cached
                        .connectors
                        .iter()
                        .filter_map(|path| {
                            let style = cached.model.map.get(path.child)?.style;
                            #[expect(
                                clippy::cast_possible_truncation,
                                reason = "a mind map's own space is screen-scale"
                            )]
                            let points: Vec<[f32; 2]> = path
                                .points
                                .iter()
                                .map(|p| [(p.x * scale) as f32, (p.y * scale) as f32])
                                .collect();
                            Some((points, style.connector, style.connector_width * scale))
                        })
                        .collect();
                    (scale, nodes, branches)
                };

                // `position` is already camera-relative — the same trap the chart arm
                // records — so a map-local offset is a plain addition.
                let at = |x: f64, y: f64| [position[0] + x as f32, position[1] + y as f32];
                let tint = |c: vellum_mindmap::Color| {
                    Rgba::from_rgb8(c.r, c.g, c.b).with_alpha(f32::from(c.a) / 255.0 * opacity)
                };

                // Branches under the nodes: a link runs to a node's border, and a node
                // with a fill should cover the last hairline of it rather than be cut.
                for (points, colour, width) in &branches {
                    if !colour.is_visible() {
                        continue;
                    }
                    #[expect(
                        clippy::cast_possible_truncation,
                        reason = "a branch's width is screen-scale"
                    )]
                    let mesh = crate::mesh::ribbon(points, (*width as f32).max(MIN_BRANCH_WIDTH));
                    push_local_mesh(list, position, &mesh, tint(*colour), rotation);
                }

                for (rect, style) in &nodes {
                    // An unstyled node is text on a branch rather than a box —
                    // `NodeStyle`'s default is transparent both ways and its own test
                    // pins that — so a node with neither fill nor border draws nothing
                    // and costs no quad.
                    if !style.fill.is_visible() && !style.border.is_visible() {
                        continue;
                    }
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

                stats.mindmaps += 1;
            }

            ItemKind::Kanban { .. } => {
                // The same layout the text pass shaped against. Copied out before the
                // pushes, as every other structured arm does.
                let (columns, cards, overflowing) = {
                    let cached = self.kanban_layout(id, projected, projected.generation);
                    let columns: Vec<_> = cached
                        .layout
                        .columns
                        .iter()
                        .map(|c| (c.rect, c.header, c.wip.limit.is_some_and(|l| c.wip.count > l)))
                        .collect();
                    let cards: Vec<_> = cached
                        .layout
                        .columns
                        .iter()
                        .flat_map(|c| c.cards.iter().map(|card| card.rect))
                        .collect();
                    // A breach is state, not colour — `vellum-flow` says so explicitly
                    // and holds no hex literal — so the decision of what a breach *looks*
                    // like is made here, against the theme.
                    (columns, cards, cached.layout.overflows())
                };

                // `position` is already camera-relative, as above.
                let at = |r: vellum_flow::Rect| {
                    (
                        [position[0] + r.left() as f32, position[1] + r.top() as f32],
                        [r.width() as f32, r.height() as f32],
                    )
                };
                let hairline = (f64::from(HAIRLINE) / camera.zoom()) as f32;

                // The board's own ground, then a column's, then its cards — three
                // surfaces a step apart, which is how the design language asks for
                // hierarchy: a luminance step and a hairline, not a shadow.
                list.push_quad(
                    QuadInstance::solid(position, size, theme.frame_fill)
                        .with_corner_radius(STICKY_RADIUS)
                        .with_border(theme.border, hairline)
                        .with_rotation(rotation)
                        .with_opacity(opacity),
                );
                for (rect, header, over) in &columns {
                    let (origin, extent) = at(*rect);
                    list.push_quad(
                        QuadInstance::solid(origin, extent, theme.canvas)
                            .with_corner_radius(STICKY_RADIUS)
                            .with_rotation(rotation)
                            .with_opacity(opacity),
                    );
                    // A breached column is marked on its header rather than by tinting
                    // the whole column: the accent is meant to be scarce, and the header
                    // is where the count that breached is written.
                    let (origin, extent) = at(*header);
                    let tint = if *over { theme.accent } else { theme.border };
                    list.push_quad(
                        QuadInstance::solid(origin, [extent[0], hairline.max(1.0)], tint)
                            .with_rotation(rotation)
                            .with_opacity(opacity),
                    );
                }
                for rect in &cards {
                    let (origin, extent) = at(*rect);
                    list.push_quad(
                        QuadInstance::solid(origin, extent, theme.frame_fill)
                            .with_corner_radius(STICKY_RADIUS)
                            .with_border(theme.border, hairline)
                            .with_rotation(rotation)
                            .with_opacity(opacity),
                    );
                }
                if overflowing {
                    // `vellum-flow` lays overflow out past the edge on purpose and
                    // nothing here scrolls, so this draws outside its own item. Logged
                    // rather than clamped: shrinking the columns is the behaviour that
                    // crate explicitly refuses, and resizing the item is the remedy.
                    log::debug!("a kanban board overflows its box; nothing here scrolls");
                }

                stats.kanbans += 1;
            }

            // The four Agent Canvas kinds — `docs/07-agent-canvas.md` features 1, 2 and 14.
            //
            // One arm, because all four are a card with pieces laid on it and every piece
            // comes from [`NodePaint`]; what differs is the *chrome*, which is matched on
            // below. Four arms would have put the card, the plate loop and the image loop in
            // four places that then have to agree about all three.
            ItemKind::Agent { .. }
            | ItemKind::FileTree { .. }
            | ItemKind::AgentNote { .. }
            | ItemKind::Browser { .. } => {
                let hairline = (f64::from(HAIRLINE) / camera.zoom()) as f32;
                let (w, h) = projected.item.placement.scaled_size();
                // A node's rectangles are in the item's own space with its top-left at
                // `(0, 0)` and are **already scaled**, and this list is in the board view
                // where a unit is a world unit — so placing one is a plain addition and
                // nothing needs a scale factor. Multiplying by `camera.zoom()` here is the
                // mistake `card_layout`'s own note records, and it looks *nearly* right.
                let at = |rect: NodeRect| {
                    (
                        [position[0] + rect.x as f32, position[1] + rect.y as f32],
                        [rect.width as f32, rect.height as f32],
                    )
                };

                // **This node's own palette**, if it is an agent and has chosen a chat theme.
                // One substitution, and every colour below follows — the card, the wells, the
                // primary and muted text, the accent rail — because they all resolve through
                // a `&Theme` already. `Theme::for_chat` answers `theme` unchanged for
                // `ChatTheme::Velm`, so a board that has never chosen one pays a compare.
                let theme = chat_theme_of(&projected.item.kind, ctx).map_or(theme, |chat| theme.for_chat(chat));
                // …and how see-through its paper is. The **paper only**: `opacity` fades the
                // whole item including the words, which at anything under about 60% is a
                // transcript nobody can read. This is what *"adjust the transparency"* means
                // on a surface whose entire job is to carry text.
                let paper = opacity * chat_opacity_of(&projected.item.kind);

                // The card first. It is what all four are built on, and it is what makes a
                // freshly placed node a real, selectable, movable box before anything at all
                // has filled it.
                let fill = projected.item.style.fill.map_or(theme.surface, theme::convert);
                push_node_card(list, position, size, fill, rotation, paper, &theme, hairline);

                // The user's own picture behind the transcript, if this node has one —
                // *"put my own themes a picture as a theme"*. Between the card and the
                // plates, so the wells and the words still sit on top of it, and it inherits
                // the card's radius so it cannot square off the corners it fills.
                if let Some(blob) = chat_background_of(&projected.item.kind)
                    && let Some((texture, source)) =
                        assets.texture(device, queue, renderer.textures_mut(), blob)
                {
                    renderer.textures_mut().mark(texture, 0.0);
                    list.push_image(
                        texture,
                        // Cropped, never stretched — feedback 23. A wallpaper is whatever
                        // shape the file was and a node is whatever shape it was dragged to.
                        ImageInstance::new(
                            position,
                            size,
                            cover_uv(source, (f64::from(size[0]), f64::from(size[1]))),
                        )
                        .with_corner_radius(CARD_RADIUS)
                        .with_rotation(rotation)
                        .with_opacity(paper),
                    );
                    // A scrim, in the theme's own paper. This is what makes an arbitrary
                    // picture safe where an arbitrary *colour* is not: the ink's contrast is
                    // against the paper above the image rather than against the image, so a
                    // photograph cannot make the transcript unreadable. It is also why the
                    // picture is a per-node choice and the colours are four presets.
                    list.push_quad(
                        QuadInstance::solid(position, size, fill.with_alpha(BACKGROUND_SCRIM))
                            .with_corner_radius(CARD_RADIUS)
                            .with_rotation(rotation)
                            .with_opacity(paper),
                    );
                }

                // Plates under the words, then pictures, then the chrome on top. Copied out
                // of the cache first because `assets` borrows the renderer and this borrow
                // of `self` would otherwise still be live across it.
                let (plates, images, twisties) = {
                    let paint = self.node_paint(id, projected, ctx);
                    (paint.plates.clone(), paint.images.clone(), paint.twisties.clone())
                };
                for plate in &plates {
                    push_node_plate(list, at(plate.rect), *plate, rotation, paper, &theme, hairline);
                }
                for image in &images {
                    let (origin, extent) = at(image.rect);
                    match assets.texture(device, queue, renderer.textures_mut(), &image.blob) {
                        Some((texture, source)) => {
                            renderer.textures_mut().mark(texture, 0.0);
                            list.push_image(
                                texture,
                                // Cropped to its band rather than stretched into it. An
                                // agent's screenshot is whatever shape its window was and the
                                // band is not, so `UvRect::FULL` here is feedback 23's *"the
                                // images are all distoreted"* waiting to happen a second time.
                                ImageInstance::new(
                                    origin,
                                    extent,
                                    cover_uv(source, (image.rect.width, image.rect.height)),
                                )
                                .with_corner_radius(CARD_RADIUS * 0.75)
                                .with_rotation(rotation)
                                .with_opacity(opacity),
                            );
                        }
                        None => {
                            stats.images_pending += 1;
                            push_placeholder(list, origin, extent, rotation, opacity, &theme);
                        }
                    }
                }

                // Everything below is positioned from the **unrotated** box and spun in
                // place, which is the convention every structured widget here follows: a
                // rotated table draws its grid straight, and the press path agrees with what
                // is on screen rather than with what would be tidier.
                match &projected.item.kind {
                    ItemKind::Agent { model, .. } => {
                        let laid = crate::agent::layout(w, h);
                        // The status the runtime reports, or `Idle` for a node it has not
                        // attached a session to — which is what a freshly placed agent is.
                        let view = ctx.agents.get(id);
                        let status = view.map_or(Status::Idle, |view| view.status);
                        let colour = status_colour(status, &theme);
                        if laid.too_small {
                            push_compact_stripe(list, position, size, colour, rotation, opacity);
                        } else {
                            push_status_dot(
                                list,
                                at(laid.status),
                                colour,
                                view.is_some_and(AgentView::needs_attention),
                                rotation,
                                opacity,
                            );
                            let raw = match view {
                                Some(view) => matches!(view.mode, DisplayMode::Raw),
                                // No session yet, so the node's own configuration answers —
                                // resolved through `agent::display_mode`, which is the one
                                // place `None` means "follow the app-wide default".
                                None => matches!(
                                    crate::agent::display_mode(
                                        &crate::agent::decode(model),
                                        ctx.default_display,
                                    ),
                                    DisplayMode::Raw
                                ),
                            };
                            push_mode_toggle(
                                list, at(laid.mode), raw, rotation, opacity, &theme, hairline,
                            );
                            push_run_button(
                                list,
                                at(laid.run),
                                status.is_busy(),
                                rotation,
                                opacity,
                                &theme,
                                hairline,
                            );
                        }
                    }

                    ItemKind::FileTree { .. } => {
                        for (rect, expanded) in &twisties {
                            push_twisty(list, at(*rect), *expanded, rotation, opacity, &theme);
                        }
                    }

                    ItemKind::Browser { .. } => {
                        let laid = crate::browser::layout(w, h);
                        if !laid.too_small {
                            push_reload_button(
                                list, at(laid.reload), rotation, opacity, &theme, hairline,
                            );
                            // **The card's own badge, not a second drawing of one.** Same
                            // plate, same three-quad `↗` — see `push_open_badge` for why the
                            // arrow is geometry rather than U+2197 — so the one button on the
                            // board that leaves the application looks the same wherever it
                            // appears, and there is one hitbox to keep honest rather than two.
                            let pixel = (1.0 / camera.zoom()) as f32;
                            push_open_badge(
                                list,
                                at(laid.open_external),
                                rotation,
                                opacity,
                                &theme,
                                ctx.hovered_badge == Some(id),
                                pixel,
                            );
                        }
                    }

                    // A note draws its title and body as runs and needs no chrome.
                    _ => {}
                }
            }

            // A group is a container, not a drawing. `vellum_doc::ItemKind::Group`
            // is explicit that it has no visual payload of its own.
            ItemKind::Group => {}

            ItemKind::Image { asset_id, crop } => {
                match assets.texture(device, queue, renderer.textures_mut(), asset_id) {
                    Some((texture, source)) => {
                        renderer.textures_mut().mark(texture, 0.0);
                        let uv = match crop {
                            Some(crop) => {
                                assets::crop_uv(*crop, (f64::from(source.0), f64::from(source.1)))
                            }
                            None => vellum_render::UvRect::FULL,
                        };
                        list.push_image(
                            texture,
                            ImageInstance::new(position, size, uv)
                                .with_rotation(rotation)
                                .with_opacity(opacity),
                        );
                    }
                    None => {
                        stats.images_pending += 1;
                        push_placeholder(list, position, size, rotation, opacity, &theme);
                    }
                }
            }

            ItemKind::Ink { color, thickness, points } => {
                let stroke_color = color.map_or(theme.stroke, theme::convert);
                // **The item's own scale belongs in the band, and its absence was invisible
                // on every stroke this app drew itself.**
                //
                // The mesh is tessellated in stroke-local space and then magnified on the
                // GPU by `placement.scale` (see the transform below), so what a viewer
                // actually sees is `zoom × scale` — while the tolerance was derived from the
                // zoom alone. Velm's own strokes carry scale 1.0, so they were always right;
                // **imported Miro ink does not** (`vellum_import::pipeline` keeps the scale
                // out of the points on purpose, because the renderer applies it), so a stroke
                // at scale 2 was tessellated exactly twice as coarsely as it is drawn, at
                // scale 4 four times. The user's comparison was of an imported board, which
                // is why the difference was so much starker than a stroke drawn here.
                let band = lod_band(camera.zoom() * projected.item.placement.scale);
                let generation = projected.generation;
                let entry = self.ink.entry(id).or_insert_with(|| CachedInk {
                    generation,
                    band,
                    mesh: tessellate_ink(points, *thickness, band),
                });
                if entry.generation != generation || entry.band != band {
                    entry.generation = generation;
                    entry.band = band;
                    entry.mesh = tessellate_ink(points, *thickness, band);
                }

                // The stroke's points are relative to the item's centre, which is
                // also what the placement is — so the transform is the centre, and
                // the vertices never carry the board's absolute extent.
                let centre = WorldPoint::new(projected.item.placement.x, projected.item.placement.y);
                let transform = list.meshes_mut().push_transform(MeshTransform::scale_rotate_at(
                    projected.item.placement.scale as f32,
                    rotation,
                    camera.to_camera_relative(centre),
                ));
                let start = list.meshes().indices().len() as u32;
                list.meshes_mut().push_ink(
                    &entry.mesh,
                    stroke_color.with_alpha(stroke_color.a * opacity),
                    transform,
                );
                let end = list.meshes().indices().len() as u32;
                list.push_meshes(start..end);
            }

            ItemKind::Connector { color, start, end, .. } => {
                let Some(mut routed) = connector::route(
                    &projected.item.kind,
                    &projected.item.placement,
                    |target| ctx.projection.placement_of(target),
                    &self.router,
                    &[],
                ) else {
                    return;
                };
                let options = vellum_connect::TessellationOptions {
                    // A curve only has to be smooth to the pixel the viewer can see;
                    // tessellating a board-spanning bezier to 0.05 world px when it
                    // is 40 px on screen is thousands of wasted triangles.
                    tolerance: (0.5 / camera.zoom()).clamp(0.05, 64.0),
                };

                // **What this line means is derived, never stored** — `docs/07` §3, and
                // `crate::agent::link_kind` is the single derivation, asked here rather than
                // reproduced. A stored "this is an agent link" flag would be a second source
                // of truth that can disagree with the endpoints it describes, which this
                // repository has already paid for twice.
                //
                // Gated on `AgentViews::has_agents`, which is the promise that a board with
                // no agents on it costs exactly what it did before this layer existed: one
                // boolean per connector rather than two projection lookups.
                //
                // **`has_agents`, not `is_empty`.** The views map holds only the nodes that
                // are *visible*, so a connector on screen whose two agent endpoints are both
                // off screen would fall to `Plain` and the line would change style as the
                // user panned — which is worse than either style, because it reads as the
                // link being lost.
                let link = if !ctx.agents.has_agents() {
                    crate::agent::LinkKind::Plain
                } else {
                    let kind_of = |end: &vellum_doc::ConnectorEnd| {
                        end.target
                            .and_then(|target| ctx.projection.scene_id(target))
                            .and_then(|scene| ctx.projection.get(scene))
                            .map(|projected| &projected.item.kind)
                    };
                    crate::agent::link_kind(
                        kind_of(start),
                        kind_of(end),
                        start.arrowhead,
                        end.arrowhead,
                    )
                };

                let mut line = color.map_or(theme.stroke, theme::convert);
                if let Some((dash, tint)) = agent_link_look(link, &theme) {
                    // The user's own colour still wins if they set one — a connector they
                    // deliberately made red stays red — but a link that has never been
                    // coloured takes the one that says which relationship it is.
                    if color.is_none() {
                        line = tint;
                    }
                    // **The cadence rides on the thickness**, because `vellum_connect`
                    // derives its dash pattern from it (`LineStyle::dash_pattern`). Setting
                    // the thickness in device pixels therefore makes the *pattern* screen-
                    // constant as well as the weight, which is the whole requirement: a
                    // world-unit cadence is a solid smear at a fitted 4% zoom and three
                    // dashes across the window at 8×, exactly as `push_grid` records for the
                    // board's own dots. It also keeps the arrowhead — which scales with
                    // thickness — a constant size, and arrowheads are how `docs/07` §3 says
                    // direction is expressed, so losing them was never an option. Doing our
                    // own dashing would have.
                    routed.style.thickness = AGENT_LINK_WIDTH / camera.zoom();
                    routed.style.line = dash;
                    // …unless that would cost more dashes than a frame should spend. A link
                    // between two far-apart agents is mostly off screen and `vellum_connect`
                    // has no viewport to clip against, so the cadence is dropped rather than
                    // stretched: still the link's colour, still legible as an agent link, and
                    // a rhythm nobody can see says nothing worth thousands of triangles.
                    // `GUIDE_MAX_DASHES` is the same backstop for the same arithmetic.
                    if let Some(pattern) = dash.dash_pattern(routed.style.thickness) {
                        let period = pattern.on + pattern.off;
                        if period > 0.0 && routed.path.length() / period > AGENT_LINK_MAX_DASHES {
                            routed.style.line = vellum_connect::LineStyle::Solid;
                        }
                    }
                }

                let Ok(mesh) = vellum_connect::tessellate(&routed.path, &routed.style, &options)
                else {
                    return;
                };
                let transform = list.meshes_mut().push_transform(MeshTransform::at(
                    camera.to_camera_relative(WorldPoint::new(mesh.origin.x, mesh.origin.y)),
                ));
                // `first`/`last` rather than `start`/`end`, which are this arm's two
                // `ConnectorEnd`s: shadowing them would compile and would make the next
                // reader check twice which one a name meant.
                let first = list.meshes().indices().len() as u32;
                list.meshes_mut().push_connector(
                    &mesh,
                    line.with_alpha(line.a * opacity),
                    transform,
                );
                let last = list.meshes().indices().len() as u32;
                list.push_meshes(first..last);

                // The message in flight, on top of the line it is travelling. Only ever
                // asked of a link that *is* one, and only when something is actually in
                // flight — `AgentViews::pulses` is empty on an idle board, which is what
                // makes this a lookup in a vector of length zero rather than an animation.
                if link.is_agent_link()
                    && let Some(pulse) = ctx.agents.pulse(id)
                {
                    push_link_pulse(
                        list,
                        ctx,
                        &routed.path.flatten(options.tolerance),
                        pulse,
                        line,
                    );
                }
            }

            // Both card kinds draw the same way, which is the point of them carrying the same
            // display fields: the difference between a Miro `preview` and a Miro `embed` is
            // which provider it came from, not what it looks like.
            ItemKind::LinkPreview { url, thumbnail, favicon, mode, .. }
            | ItemKind::Embed { url, thumbnail, favicon, mode, .. } => {
                push_shape_card(list, position, size, rotation, opacity, &theme);

                // The same layout the two text blocks use, so the picture and the words cannot
                // end up on top of each other. See `card_layout`.
                let (w, h) = projected.item.placement.scaled_size();
                let font = card_font_size(w);
                let laid = card_layout(
                    w,
                    h,
                    font,
                    *mode,
                    thumbnail.is_some(),
                    favicon.is_some(),
                    url.is_some(),
                    url.as_deref().is_some_and(vellum_link::plays_video),
                    drawn_title_chars(&projected.item.kind),
                );
                // A box in the item's own space becomes one in whatever space this list is
                // drawing in, as a **fraction of the item**: `position` and `size` are already
                // the item's top-left and extent in that space, so the offsets scale by
                // `size / item size` and nothing needs to know what the space is.
                //
                // Multiplying by `camera.zoom()` instead is wrong and looked *nearly* right —
                // the card was correct and the image overflowed its right edge while the favicon
                // landed on top of the title. The list is in the board view here, where a unit
                // is already a world unit; the zoom is baked into `size`.
                let scale = (
                    if w > 0.0 { size[0] / w as f32 } else { 0.0 },
                    if h > 0.0 { size[1] / h as f32 } else { 0.0 },
                );
                let to_screen = |(x, y, bw, bh): (f64, f64, f64, f64)| {
                    (
                        [position[0] + x as f32 * scale.0, position[1] + y as f32 * scale.1],
                        [bw as f32 * scale.0, bh as f32 * scale.1],
                    )
                };
                // One device pixel in this list's units, for the badge's minimum stroke. See
                // `push_open_badge`'s `pixel` parameter: the list is in the board view, so a
                // literal 1.0 is one *world* unit and goes sub-pixel on any fitted board.
                let pixel = (1.0 / ctx.camera.zoom()) as f32;

                // The preview image, in the one mode that has room for it. `Card` and `Link`
                // deliberately draw no image even when one has been fetched — that is what
                // makes them smaller, and switching modes must not need a re-fetch.
                //
                // **The `else` is the fix, and it is the same one the favicon already
                // has.** This chain had none, so a card whose picture had not arrived
                // reserved the band and painted nothing into it — a white void where the
                // image goes. It was the odd one out in its own file: a standalone
                // `ItemKind::Image` has drawn `push_placeholder` all along, and the
                // favicon forty lines down has `push_favicon_placeholder`. Only the
                // biggest box on the card was left empty, which is most of what the user
                // meant by *"i just see a box"*.
                //
                // Written as one `match` on the texture rather than a residency test
                // beside the draw: `assets` is borrowed mutably by `texture`, and asking
                // twice would either need a second query API or draw the placeholder
                // *under* a translucent picture, which tints it.
                if let Some(box_) = laid.image
                    && let Some(hash) = thumbnail
                {
                    let (at, extent) = to_screen(box_);
                    match assets.texture(device, queue, renderer.textures_mut(), hash) {
                        Some((texture, source)) => {
                            renderer.textures_mut().mark(texture, 0.0);
                            list.push_image(
                                texture,
                                // Cropped to the band's aspect rather than stretched into
                                // it. The *layout* box is the right thing to measure
                                // against, not `extent`: both are the same shape, but the
                                // layout box is in world units and is not rounded to
                                // device pixels, so it does not make the crop shiver by a
                                // texel as the board is zoomed.
                                ImageInstance::new(
                                    at,
                                    extent,
                                    cover_uv(source, (box_.2, box_.3)),
                                )
                                // Its own radius, slightly tighter than the card's, which
                                // is what makes a picture read as sitting *in* the card
                                // rather than as the card's own face.
                                .with_corner_radius(CARD_RADIUS * 0.75)
                                .with_rotation(rotation)
                                .with_opacity(opacity),
                            );
                        }
                        None => push_card_image_placeholder(
                            list, at, extent, rotation, opacity, &theme,
                        ),
                    }
                }

                // The site's icon, at the head of the provider row. Miro leads every card and
                // every collapsed row with one, and it is the fastest thing on a card to read:
                // a favicon is recognised before any of the words are.
                //
                // # The `else` is the fix, and it is why the layout was left alone
                //
                // `card_layout` reserves this box on `favicon.is_some()` — on a *hash* existing,
                // which is not the same as bytes that draw. The user photographed the gap that
                // makes: an imported Alibaba card whose icon fetched fine, failed to decode
                // (it is an ICO, and the `image` crate was built without that feature), and was
                // recorded `Undecodable` for the session — so the row stayed indented around an
                // empty square with nothing in it.
                //
                // Reserving on *drawability* instead was the obvious repair and is the wrong
                // one twice over: `Painter::block` computes the same layout to place the text
                // and cannot see `Assets` — it is borrowed mutably by this very pass — and an
                // icon that arrives mid-session would then reflow the words out from under the
                // reader. A placeholder in the box that was already reserved has neither
                // problem, and it covers *pending* and *never-fetched* with the same paint
                // rather than leaving a hole for each.
                if let Some(box_) = laid.favicon
                    && let Some(hash) = favicon
                {
                    let (at, extent) = to_screen(box_);
                    if let Some((texture, source)) =
                        assets.texture(device, queue, renderer.textures_mut(), hash)
                    {
                        renderer.textures_mut().mark(texture, 0.0);
                        list.push_image(
                            texture,
                            // Cropped too. A favicon is square and its box is square, so this
                            // is almost always the identity — but "almost always" is why it is
                            // here: a site serving a wide wordmark as its icon would otherwise
                            // squash it into a square, and that is one site away rather than
                            // impossible.
                            ImageInstance::new(at, extent, cover_uv(source, (box_.2, box_.3)))
                                .with_corner_radius(FAVICON_RADIUS)
                                .with_rotation(rotation)
                                .with_opacity(opacity),
                        );
                    } else {
                        push_favicon_placeholder(list, at, extent, rotation, opacity, &theme);
                    }
                }

                // The ▶, over the poster, for a card whose page is a video.
                if let Some(box_) = laid.play {
                    let hovered = ctx.hovered_badge == Some(id);
                    push_play_button(list, to_screen(box_), rotation, opacity, &theme, hovered);
                }

                // The open-page badge, last, so it is on top of the picture it sits over.
                if let Some(box_) = laid.badge {
                    let hovered = ctx.hovered_badge == Some(id);
                    push_open_badge(list, to_screen(box_), rotation, opacity, &theme, hovered, pixel);
                }
            }

            ItemKind::Document { .. } => {
                push_shape_card(list, position, size, rotation, opacity, &theme);
            }
        }

        // Text last. Glyphs go in the screen view, so they are rasterised at the size
        // they occupy rather than scaled by the camera; greeked bars stay in the board
        // view, where they coalesce with the geometry above instead of paying for the
        // view flip.
        // The prompt row's caret, resolved **once** rather than per slot. Gated on the view
        // carrying one, which is true for at most one node on the board — so no ordinary item
        // is asked to build a `NodePaint` it does not have.
        let mut prompt_slot = None;
        if ctx.agents.get(id).is_some_and(|view| view.caret.is_some()) {
            prompt_slot = self.node_paint(id, projected, ctx).prompt_slot();
        }
        for slot in 0..self.slots_of(id, projected, projected.generation, ctx) {
            // Either caret, as one `TextCursor`. An agent's prompt row is a text surface like
            // any other and gets the same caret, the same selection wash and the same blink —
            // a second implementation drawn only for prompts is two things to keep in step.
            let caret = ctx
                .editing
                .filter(|c| c.scene == id && c.slot == slot)
                .or_else(|| prompt_cursor(id, slot, prompt_slot, ctx));
            match self.block(id, projected, slot, ctx) {
                Some(Painted::Glyphs(block)) => {
                    list.use_view(screen);
                    let atlas = renderer.atlas();
                    let layout = self
                        .text
                        .layout_of(BlockKey::new(id, slot))
                        .expect("the block was just laid out");
                    // The selection goes *under* the glyphs, so the text stays legible
                    // through it, and the caret goes over — which is only visible where
                    // the two coincide, at a caret sitting on a glyph's stem.
                    if let Some(cursor) = caret {
                        push_selection_boxes(list, ctx, &block, layout, cursor);
                    }
                    stats.glyphs_missing += list.push_layout(
                        atlas,
                        layout,
                        [block.origin.x as f32, block.origin.y as f32],
                        // The block's own scale, not the camera's. They are the same for
                        // everything except a frame's title, which refuses to shrink past
                        // legibility — see `Block::scale`.
                        block.scale,
                        block.color.with_alpha(block.color.a * opacity),
                    );
                    if let Some(cursor) = caret {
                        push_caret(list, ctx, &block, layout, cursor);
                        // Kept for the *next* click to resolve against. See the field.
                        self.edited_origin = Some((BlockKey::new(id, slot), block.origin, block.scale));
                    }
                }
                Some(Painted::Greeked(greek)) => {
                    list.use_view(board);
                    self.push_greek(list, camera, &greek, opacity);
                }
                None => {}
            }
        }
    }

    /// Emits the bars that stand in for text too small to read.
    ///
    /// Board view, so they coalesce into the run of quads the items around them are
    /// already pushing — a greeked item is *cheaper* than a drawn one, not dearer.
    fn push_greek(&self, list: &mut DrawList, camera: &Camera, greek: &Greek, opacity: f32) {
        let color = greek.color.with_alpha(greek.color.a * GREEK_ALPHA * opacity);
        match greek.lines {
            // Nothing shaped, or the lines are too close to tell apart: one bar for
            // the whole block.
            GreekLines::Single { width, height } => {
                let origin = camera.to_camera_relative(greek.origin);
                list.push_quad(QuadInstance::solid(origin, [width as f32, height as f32], color));
            }
            // Real lines, read straight off the cached layout: the bars sit where the
            // lines sit and are as wide as the lines are, so a wrapped paragraph keeps
            // its ragged right edge.
            GreekLines::PerLine { key, bar } => {
                let Some(layout) = self.text.layout_of(key) else { return };
                for line in &layout.lines {
                    let width = f64::from(line.width).min(greek.column);
                    if width <= 0.0 {
                        continue;
                    }
                    // `LaidOutLine::width` is the advance before alignment, so the
                    // block's own alignment has to be reapplied here or a centred
                    // sticky greeks hard against its left edge.
                    let indent = match greek.align {
                        Align::Center => (greek.column - width) / 2.0,
                        Align::Right => greek.column - width,
                        Align::Left => 0.0,
                    };
                    // Centre the bar in its line box: an x-height stripe sitting on
                    // the line's own baseline band.
                    let slack = (f64::from(line.height) - bar).max(0.0) / 2.0;
                    let at = WorldPoint::new(
                        greek.origin.x + indent,
                        greek.origin.y + f64::from(line.top) + slack,
                    );
                    let origin = camera.to_camera_relative(at);
                    list.push_quad(QuadInstance::solid(origin, [width as f32, bar as f32], color));
                }
            }
            // A block that was never shaped, so the bars are laid on the type's own rhythm.
            GreekLines::Estimated { lines, bar, spacing, width } => {
                for line in 0..lines {
                    // The last bar is short, the way the last line of a paragraph is. Without
                    // it a stack of identical full-width bars reads as a table or a barcode
                    // rather than as text — which is the whole job of a greeked block, since
                    // nobody can read it either way.
                    let width = if line + 1 == lines && lines > 1 { width * 0.62 } else { width };
                    let slack = (spacing - bar).max(0.0) / 2.0;
                    let indent = match greek.align {
                        Align::Center => (greek.column - width) / 2.0,
                        Align::Right => greek.column - width,
                        Align::Left => 0.0,
                    };
                    let at = WorldPoint::new(
                        greek.origin.x + indent,
                        greek.origin.y + line as f64 * spacing + slack,
                    );
                    let origin = camera.to_camera_relative(at);
                    list.push_quad(QuadInstance::solid(origin, [width as f32, bar as f32], color));
                }
            }
        }
    }

    /// Lays out one of an item's text slots and decides what it contributes.
    ///
    /// Returns `None` when the slot is empty for this kind, which is the common case
    /// — most items have no secondary block and several have no text at all — and
    /// [`Painted::Greeked`] when the text is real but too small to read, which is what
    /// stops a zoomed-out board from looking empty.
    ///
    /// This is the *only* place the readable-size decision is made. It used to be
    /// taken here and then re-taken by both callers against
    /// [`MIN_DEVICE_FONT_SIZE`], which is exactly the sort of triplicated threshold
    /// that drifts.
    fn block(
        &mut self,
        id: SceneId,
        projected: &Projected,
        slot: u16,
        ctx: &DrawContext<'_>,
    ) -> Option<Painted> {
        let generation = projected.generation;
        let key = BlockKey::new(id, slot);
        let (_, (width, height)) = projected.rect();
        let placement = &projected.item.placement;
        let theme = ctx.theme;

        // A table's cells come first, because they are the one case keyed by an index
        // rather than by a named slot.
        if let ItemKind::Table { .. } = &projected.item.kind
            && slot >= CELL_SLOT_BASE
        {
            return self.table_cell_block(id, projected, slot, ctx);
        }
        if let ItemKind::MindMap { .. } = &projected.item.kind
            && slot >= CELL_SLOT_BASE
        {
            return self.mindmap_node_block(id, projected, slot, ctx);
        }
        if let ItemKind::Kanban { .. } = &projected.item.kind
            && slot >= CELL_SLOT_BASE
        {
            return self.kanban_run_block(id, projected, slot, ctx);
        }
        // An agent's transcript, a note's body, a tree's rows, a browser's address: all
        // flattened into one run list, so a slot is an index into it exactly as a kanban's
        // is. See [`NodeRun`].
        if is_node(&projected.item.kind) && slot >= CELL_SLOT_BASE {
            return self.node_run_block(id, projected, slot, ctx);
        }

        // Set by the card arm below; unused by every other kind.
        let mut card_line_budget = usize::MAX;
        // Set by the frame-title arm below; zero — meaning "no floor" — for every other kind.
        // See `FRAME_TITLE_MIN_DEVICE` and `Block::scale`.
        let mut min_device_size = 0.0_f64;
        // Whether the caret is in *this* slot right now.
        //
        // The arms below skip a slot with no words in it, which is right — a fresh table
        // costs nothing to shape — and catastrophic while a caret is in one: no block means
        // no origin, and `push_caret` has nothing to draw against, so the caret simply does
        // not appear. `table_cell_block` and `kanban_run_block` were each taught this
        // separately; a **sticky, a text box and a frame title were not**, which is
        // *"when i double click on the notepad the writing status symbol … still does not
        // work … it only starts flashing after i start typing"*: the first keystroke gives
        // the item words, the words give it a block, and the block is what the caret was
        // waiting for all along.
        let caret_here = ctx.editing.is_some_and(|c| c.scene == id && c.slot == slot);
        let (fit, anchor, color, style) = match (&projected.item.kind, slot) {
            // A note's text is centred in the note, both ways — Miro's own vertical
            // centring, which is what makes a one-word sticky look deliberate
            // rather than top-heavy.
            // A shape's label is centred in it, both ways, for the same reason a
            // sticky's is: a shape is a container for one short phrase, and a phrase
            // pinned to the top-left of a diamond reads as a mistake.
            (ItemKind::Sticky { text, .. } | ItemKind::Shape { text, .. }, BlockKey::PRIMARY) => {
                if text.is_empty() && !caret_here {
                    return None;
                }
                (
                    text::sticky_fit(width, height),
                    Anchor::Centred,
                    projected
                        .item
                        .style
                        .text_color
                        .map_or(theme.text, theme::convert),
                    projected.item.style.clone(),
                )
            }

            (ItemKind::Text { text }, BlockKey::PRIMARY) => {
                if text.is_empty() && !caret_here {
                    return None;
                }
                (
                    FitBox::new(width.max(1.0) as f32, height.max(1.0) as f32),
                    Anchor::TopLeft,
                    projected
                        .item
                        .style
                        .text_color
                        .map_or(theme.text, theme::convert),
                    projected.item.style.clone(),
                )
            }

            // **An agent's role and a note's title are the item's own `StyledText`**, which
            // is precisely what `docs/07` §2 keeps them beside the token for: it puts both
            // inside search, the on-canvas caret and `Board::set_text` with no new path.
            // Everything else either node draws is a run, because it is not the user's text.
            //
            // The caret exception is the sticky's, and it has to be restated rather than
            // assumed — feedback 25 is the record of what happens when an exception is taught
            // at two call sites out of three: an empty slot holding the cursor must still
            // produce a block, or there is no origin to draw the caret against and
            // double-clicking a blank role does nothing at all.
            (
                ItemKind::Agent { label: words, .. } | ItemKind::AgentNote { title: words, .. },
                BlockKey::PRIMARY,
            ) => {
                if words.is_empty() && !caret_here {
                    return None;
                }
                let size = placement.scaled_size();
                let font = node_font_size(size.0);
                let (x, y, box_w, box_h) = node_title_box(&projected.item.kind, size, font);
                (
                    FitBox::new(box_w.max(1.0) as f32, box_h.max(1.0) as f32),
                    Anchor::Inset(x, y),
                    projected
                        .item
                        .style
                        .text_color
                        .map_or(theme.text, theme::convert),
                    Style {
                        font_size: Some(font * ROLE_SCALE),
                        line_height: Some(NODE_LINE_HEIGHT),
                        align: Some(Align::Left),
                        ..projected.item.style.clone()
                    },
                )
            }

            (ItemKind::Frame { title, .. }, BlockKey::SECONDARY) => {
                if title.is_empty() && !caret_here {
                    return None;
                }
                // ⚠ Asked, not re-derived. The comment above `TEXT_LAYOUT_BUDGET` has said
                // this expression is `vellum_project::look::frame_title_size` "shared with the
                // browser painter" since the constants moved — and it was three constants
                // inlined here, so the function had *no caller in this crate at all* and the
                // sharing was a claim rather than a fact.
                let size = frame_title_size(height);
                // The shaped size stays in world units — the floor is applied to the *draw
                // scale* instead, below, for the cache reason `Block::scale` records.
                min_device_size = FRAME_TITLE_MIN_DEVICE;
                (
                    FitBox::new(width.max(1.0) as f32, size as f32),
                    Anchor::Above(size),
                    theme.text_muted,
                    Style { font_size: Some(size), ..projected.item.style.clone() },
                )
            }

            (
                ItemKind::LinkPreview { .. } | ItemKind::Embed { .. } | ItemKind::Document { .. },
                BlockKey::PRIMARY | BlockKey::SECONDARY | CARD_BLURB_SLOT,
            ) => {
                // The image band is reserved only when there is an image to put in it. A
                // `Large` card with nothing fetched yet would otherwise draw its text halfway
                // down over 55% of empty surface, which reads as a broken card rather than as
                // one waiting for a picture — and every card on a freshly imported board is in
                // exactly that state.
                let mode = match card_mode(&projected.item.kind) {
                    CardMode::Large if !has_thumbnail(&projected.item.kind) => CardMode::Card,
                    other => other,
                };
                // **An explicit size, not auto-fit.** A card's text is a *label* at a fixed
                // size, the way it is in a browser's link preview and in Miro's own card —
                // whereas auto-fit asks "how big can this be and still fit", which for a
                // collapsed row holding six words answers 40pt and fills the whole box. That
                // is what the first version of this drew.
                //
                // Scaled by the card's width so a deliberately enlarged card enlarges its
                // text with it, rather than keeping 13pt type in a 1000-unit box.
                let size = card_font_size(width);
                let laid = card_layout(
                    width,
                    height,
                    size,
                    mode,
                    has_thumbnail(&projected.item.kind),
                    has_favicon(&projected.item.kind),
                    has_link(&projected.item.kind),
                    is_video(&projected.item.kind),
                    drawn_title_chars(&projected.item.kind),
                );
                // **Three slots, because a layout carries one colour and one size.** Miro's
                // card is a muted site name, then a large title, then a smaller grey blurb.
                // `DrawList::push_layout` takes a single colour for a whole run and `Style`
                // a single `font_size`, and `SpanStyle` has no size field at all — so each
                // tone-and-size pairing has to be its own block. Doing it in one would need
                // per-span colour *and* per-span size in the glyph pass, which is a renderer
                // change for a card.
                let (x, y, w, h) = match slot {
                    BlockKey::SECONDARY => laid.provider,
                    CARD_BLURB_SLOT => laid.blurb,
                    _ => laid.title,
                };
                // Each block is set at its own size, which is the half that makes the title
                // read from a distance — colour alone left the card looking like one paragraph.
                //
                // **Three sizes, not two.** The site name used to be the base size: identical
                // to the title and *larger* than the blurb, so the card's largest text was the
                // one thing on it that matters least. See `PROVIDER_SCALE`.
                let size = match slot {
                    BlockKey::SECONDARY => size * PROVIDER_SCALE,
                    CARD_BLURB_SLOT => size * BLURB_SCALE,
                    _ => size * TITLE_SCALE,
                };
                // How many characters this block can hold, which is what the text is clipped
                // to. Estimated from the advance rather than measured: measuring needs a shaping
                // pass, and this decides *what to shape*. Half the font size is the mean advance
                // of a proportional face to within a few percent, and the clip ends in an
                // ellipsis either way — a character out is invisible.
                //
                // **Clipped rather than allowed to overflow.** A card is a fixed box and a
                // page's blurb is any length: without this, a forum page with a 400-character
                // description drew its text straight out through the bottom of the card and on
                // to the board, over whatever was beneath it.
                // The same three statements `vellum_project::card::line_budget` is, inlined
                // here and now asked instead — a third copy of the one derivation that decides
                // how much of a blurb is shaped.
                card_line_budget = line_budget(w, h, size);
                (
                    FitBox::new(w.max(1.0) as f32, h.max(1.0) as f32),
                    Anchor::Inset(x, y),
                    // The site name and the blurb are muted and the title is not, which is
                    // most of what makes Miro's card read as a card rather than as a paragraph.
                    if slot == BlockKey::PRIMARY { theme.text } else { theme.text_muted },
                    Style {
                        font_size: Some(size),
                        line_height: Some(CARD_LINE_HEIGHT),
                        align: Some(Align::Left),
                        ..projected.item.style.clone()
                    },
                )
            }

            _ => return None,
        };

        // Bail out *before* shaping anything the viewer could not read.
        //
        // This is the difference between opening the reference board in a frame and
        // opening it in a minute. Fitting the board puts it at 4% zoom, where all 236
        // of its text blocks are sub-pixel — and finding that out by auto-fitting each
        // one costs a dozen shaping passes apiece. `largest_font_size` is a bound that
        // needs no layout: an auto-fitted block never exceeds the box it must fit
        // into, and an explicit size is already known.
        //
        // # The bars are laid on the *type's* rhythm, not on the box
        //
        // This used to emit one bar of `0.45 × fit.height`, defended as "the whole block is
        // under `MIN_DEVICE_FONT_SIZE` tall, so there is no room for a second bar anyone
        // could tell apart". That premise holds only where `largest_font_size` **is**
        // `fit.height` — an auto-fitted block, which is what a sticky is. A link card sets
        // an *explicit* 13-unit size, so the guard tested 13 while the bar was drawn from a
        // ~142-unit body box: a grey slab a third of the card tall, and the user's *"on Miro
        // it's easier to see the titles, on Velm I have to zoom in a lot more"*.
        //
        // So the size the guard tested is the size the bars are built from. `Estimated`
        // stacks them without shaping anything, which is what the guard exists to protect.
        // **A box with no room draws nothing**, and it has to be said here rather than left to
        // the arithmetic. `card_layout` snaps its title and blurb down to a whole number of
        // lines and hands back a zero when a card is too short for one — the fix for *"the
        // writing falls out of the card"* — but `FitBox::new(w.max(1.0), h.max(1.0))` clamps
        // that zero back up to a full point, and the line-capacity estimate below floors at
        // `max(1)`. So a box that was deliberately emptied still shaped and drew a line, which
        // is the overflow arriving by the very route that was meant to close it.
        //
        // A caret is the exception, and the same one `Painter::block` already makes for a
        // wordless sticky: an empty slot holding the cursor must still produce a block, or
        // there is no origin to draw the caret against.
        if fit.height <= 0.0 && !caret_here {
            return None;
        }

        let size = largest_font_size(&style, fit);

        // **The floor, applied to magnification rather than to the shaped size.**
        //
        // A frame's name is the label that says where you are on a zoomed-out board, so it is
        // exactly the text that must survive being zoomed out — and it was the one thing on the
        // canvas that could not. At a default 1600x900 frame it is 27 world units, which is
        // 6.75 device pixels at 25% zoom and greeks below 18.5%, where it became *one grey bar
        // as wide as the whole frame*. That smudge is what the user photographed beside Miro's
        // perfectly legible "Education" and "Food".
        //
        // The comment on `FRAME_TITLE_FRACTION` used to assert that Miro scales a frame's name
        // with the frame rather than with the zoom. Their own screenshot disproves it: Miro
        // clamps. The belief is what produced the bug, so it was rewritten rather than tuned.
        //
        // Identity for every other kind — `min_device_size` is zero, and `max` with the zoom
        // leaves the zoom.
        let draw_scale = if min_device_size > 0.0 && size > 0.0 {
            ctx.camera.zoom().max(min_device_size / size)
        } else {
            ctx.camera.zoom()
        };
        // The gap above a frame is expressed in font sizes (`Anchor::Above`), in **world**
        // units. Magnified glyphs over an unmagnified gap sit on the frame's own top edge, so
        // the offset has to grow by exactly what the type did. A no-op when the two agree.
        let anchor = match anchor {
            Anchor::Above(by) if ctx.camera.zoom() > 0.0 => {
                Anchor::Above(by * draw_scale / ctx.camera.zoom())
            }
            other => other,
        };

        // **A block that cannot be shaped this frame is greeked, not dropped.**
        //
        // `TEXT_LAYOUT_BUDGET` rations shaping to 3 ms a frame, and this used to answer
        // `None` for everything past the ration — drawing *nothing at all*. On a board
        // where an edit had just invalidated every layout that meant the words vanished
        // and trickled back over many frames, reported as the app being slow to do and
        // undo edits. The bars below are the same ones the too-small guard already draws,
        // cost no shaping by construction (`GreekLines::Estimated` is sized from the type,
        // not from a layout), and say *"words are coming here"* instead of leaving a hole.
        //
        // The caret is exempt: a block being typed into must be shaped whatever the
        // budget says, or there is no origin to draw the cursor against — feedback 25,
        // arrived at from a third direction.
        let fresh = !self.text.is_current(key, generation);
        let starved = fresh && !caret_here && self.text_spent >= TEXT_LAYOUT_BUDGET;
        if starved {
            self.text_deferred += 1;
        }
        // `draw_scale`, not the raw zoom: a frame title is drawn magnified below the floor, so
        // testing the camera here would greek text that is about to be 13 device pixels tall.
        if starved || size * draw_scale < f64::from(MIN_DEVICE_FONT_SIZE) {
            let zoom = ctx.camera.zoom();
            let spacing = size * style.line_height.unwrap_or(CARD_LINE_HEIGHT);
            let width = f64::from(fit.width);
            let height = f64::from(fit.height);
            // Bars closer together than they are thick are a smear, not a paragraph — the
            // same collapse the post-shape path applies, for the same reason, and it is what
            // keeps a 4%-zoom board from emitting a stack of sub-pixel quads per item.
            let lines = if spacing > 0.0 && spacing * zoom >= GREEK_LINE_SPACING_MIN {
                // **The same relative nudge `whole_lines` needs, for the same reason and one
                // conversion further on.** A card's title box is built as an exact multiple of
                // this spacing, and then stored in a `FitBox`, which is `f32`: two lines of
                // 17.55 come back as 35.099998474121094, `/ 17.55` is 1.9999999, and `floor`
                // answers **one**. The block then drew a single bar sized from the whole box —
                // the exact slab this branch exists to avoid, reintroduced by a rounding error
                // rather than by the arithmetic.
                //
                // Measured: it appeared the moment the title box stopped always being three
                // lines, because 3 × 17.55 happens to round *up* in `f32` and 2 × 17.55 down.
                // A box that is an exact number of lines must be read as that many.
                const SNAP: f64 = 1e-6;
                #[expect(clippy::cast_sign_loss, reason = "both are positive here")]
                #[expect(clippy::cast_possible_truncation, reason = "clamped to a small count")]
                let count =
                    ((height / spacing) + SNAP).floor().clamp(1.0, MAX_GREEK_LINES) as usize;
                count
            } else {
                1
            };
            // A single bar keeps standing in for the *box* rather than for a line — with the
            // lines indistinguishable there is nothing else it could honestly represent, and
            // an auto-fitted block arrives here with a box that is one line tall anyway.
            let lines = if lines > 1 {
                GreekLines::Estimated { lines, bar: greek_bar_height(spacing, zoom), spacing, width }
            } else {
                GreekLines::Single { width, height: greek_bar_height(height, zoom) }
            };
            let origin = Self::locate_world(BlockRequest {
                placement,
                block_width: width,
                block_height: height,
                anchor,
                font_size: 0.0,
                color,
                // Unread on this path — `locate_world` places the box and the bars are drawn
                // in the board view — but carried honestly rather than defaulted, so the two
                // constructions of a request cannot drift.
                scale: draw_scale as f32,
            });
            return Some(Painted::Greeked(Greek {
                origin,
                column: width,
                align: style.align.unwrap_or(Align::Left),
                color,
                lines,
            }));
        }

        // Shaping a block the cache does not hold is the expensive path, and it is
        // rationed. A block that misses out this frame simply draws no text until a
        // later one has room — never a stall, and never a wrong layout.
        let started = fresh.then(Instant::now);

        let kind = &projected.item.kind;
        let (layout, font_size) = self.text.layout(key, generation, &style, Some(fit), || {
            match (kind, slot) {
                (ItemKind::Sticky { text, .. } | ItemKind::Shape { text, .. }, _) => {
                    text::convert(text)
                }
                (ItemKind::Text { text }, _) => text::convert(text),
                (ItemKind::Frame { title, .. }, _) => text::convert(title),
                (
                    ItemKind::Agent { label: words, .. }
                    | ItemKind::AgentNote { title: words, .. },
                    _,
                ) => text::convert(words),
                _ => card_text(kind, slot, card_line_budget),
            }
        });
        let extent = layout.extent;
        // Line spacing, taken from the layout rather than recomputed from the style:
        // auto-fit chose the size, and `line_height` is a multiplier over it.
        let spacing = layout
            .lines
            .first()
            .map_or(f64::from(extent.height), |line| f64::from(line.height));
        if let Some(started) = started {
            self.text_spent += started.elapsed();
        }

        let request = BlockRequest {
            placement,
            block_width: f64::from(fit.width),
            block_height: f64::from(extent.height),
            anchor,
            font_size,
            color,
            scale: draw_scale as f32,
        };

        // Shaped, and still too small to read. The auto-fitted size is only knowable
        // *after* the fit, so this is a second and genuinely different threshold from
        // the one above: a sticky whose box clears it can easily hold 14 px text that
        // does not. These are the blocks that used to vanish silently.
        //
        // The **sibling** of the guard above, and it has to read the same scale — fixing one
        // and not the other is how a frame title would clear the pre-shape check and then greek
        // anyway, three statements later, for exactly the reason the first check was changed.
        let zoom = ctx.camera.zoom();
        if font_size * draw_scale as f32 >= MIN_DEVICE_FONT_SIZE {
            return Some(Painted::Glyphs(self.locate(ctx, request)));
        }

        let origin = Self::locate_world(request);
        let bar = greek_bar_height(spacing, zoom);
        Some(Painted::Greeked(Greek {
            origin,
            column: f64::from(fit.width),
            align: style.align.unwrap_or(Align::Left),
            color,
            // Bars closer together than they are thick are a smear, not a paragraph.
            // Collapsing keeps the mark honest and bounds the quad count: a 40-line
            // block never emits 40 sub-pixel bars.
            lines: if layout.lines.len() > 1 && spacing * zoom >= GREEK_LINE_SPACING_MIN {
                GreekLines::PerLine { key, bar }
            } else {
                GreekLines::Single {
                    width: f64::from(extent.width).min(f64::from(fit.width)),
                    height: greek_bar_height(f64::from(extent.height), zoom),
                }
            },
        }))
    }

    /// One table cell's text, as a block.
    ///
    /// Positioned from the laid-out grid rather than from the item's own box: a cell's
    /// rectangle is where `vellum-table` put it, after auto-fit sizing and any merges,
    /// and there is no way to derive that from the placement alone.
    ///
    /// Cells with no words return `None` rather than an empty block, so an empty table
    /// costs nothing to shape — which is what a freshly placed one is.
    fn table_cell_block(
        &mut self,
        id: SceneId,
        projected: &Projected,
        slot: u16,
        ctx: &DrawContext<'_>,
    ) -> Option<Painted> {
        let generation = projected.generation;
        let index = usize::from(slot - CELL_SLOT_BASE);
        let placement = projected.item.placement;
        let theme = ctx.theme;

        // Copied out of the cache before the engine is borrowed to shape: the layout
        // lives in `self.tables` and the shaping needs `self.text`.
        let cell = {
            let cached = self.table_layout(id, projected, generation);
            let cell = cached.layout.cells.get(index)?;
            // The words live on the table, reached by the cell's anchor; the layout
            // carries only where they go.
            let content = cached.table.cell(cell.anchor)?.content();
            // An empty cell is skipped so a fresh table costs nothing to shape — but not
            // while a caret is in it, or clicking into an empty cell would put the caret
            // somewhere with no block to be drawn against and it would simply not appear.
            if content.is_empty()
                && !ctx.editing.is_some_and(|c| c.scene == id && c.slot == slot)
            {
                return None;
            }
            (cell.content_rect, cell.style.text.clone(), crate::table::to_text(content))
        };
        let (rect, cell_style, content) = cell;

        // The cell's rectangle is relative to the table's own origin, and the table's
        // origin is the item's top-left — so the world position is one addition.
        let (item_w, item_h) = placement.scaled_size();
        let origin = WorldPoint::new(
            placement.x - item_w / 2.0 + rect.origin.x,
            placement.y - item_h / 2.0 + rect.origin.y,
        );

        let style = Style {
            font_family: cell_style.font_family.clone(),
            font_size: Some(cell_style.font_size),
            line_height: Some(cell_style.line_height),
            align: Some(match cell_style.align {
                vellum_table::TextAlign::Left => Align::Left,
                vellum_table::TextAlign::Center => Align::Center,
                vellum_table::TextAlign::Right => Align::Right,
            }),
            ..Style::default()
        };
        let color = cell_style.color.map_or(theme.text, |c| {
            Rgba::from_rgb8(c.r, c.g, c.b)
        });
        let fit = FitBox::new(rect.size.width.max(1.0) as f32, rect.size.height.max(1.0) as f32);

        let key = BlockKey::new(id, slot);
        let (_, font_size) =
            self.text.layout(key, generation, &style, Some(fit), || content.clone());

        // A cell is positioned absolutely rather than through `Anchor`, which describes
        // where a block hangs off an *item*. Nothing about a cell hangs off the item.
        let device = ctx.camera.world_to_screen(origin);
        Some(Painted::Glyphs(Block {
            origin: ScreenPoint::new(device.x.round(), device.y.round()),
            font_size,
            color,
            // A widget's internals draw at the camera's own zoom. Only a frame's title
            // floors its magnification — see `Block::scale`.
            scale: ctx.camera.zoom() as f32,
        }))
    }

    /// How many text slots an item has.
    ///
    /// Two for everything with a body and a label. A **table** has one per cell on top
    /// of those, and a **mind map** one per visible node, because each is an
    /// independently positioned, independently styled block — which is exactly what a
    /// slot is for.
    fn slots_of(
        &mut self,
        id: SceneId,
        projected: &Projected,
        generation: u64,
        ctx: &DrawContext<'_>,
    ) -> u16 {
        // The four Agent Canvas kinds flatten their labels the way a kanban does, so a slot
        // is an index into a run list. `ctx` is here for them alone: what they draw comes
        // from a per-frame snapshot rather than from the document, so the count cannot be
        // derived from the item.
        if is_node(&projected.item.kind) {
            let runs = self.node_paint(id, projected, ctx).runs();
            return CELL_SLOT_BASE.saturating_add(u16::try_from(runs).unwrap_or(u16::MAX));
        }
        match &projected.item.kind {
            ItemKind::Table { .. } => {
                let cells = self.table_layout(id, projected, generation).layout.cells.len();
                CELL_SLOT_BASE.saturating_add(u16::try_from(cells).unwrap_or(u16::MAX))
            }
            ItemKind::MindMap { .. } => {
                let nodes = self.mindmap_layout(id, projected, generation).layout.len();
                CELL_SLOT_BASE.saturating_add(u16::try_from(nodes).unwrap_or(u16::MAX))
            }
            ItemKind::Kanban { .. } => {
                let runs = self.kanban_layout(id, projected, generation).runs.len();
                CELL_SLOT_BASE.saturating_add(u16::try_from(runs).unwrap_or(u16::MAX))
            }
            // A card's blurb is a third block — see `CARD_BLURB_SLOT`. It reuses the first
            // index past the two named slots, which is free here because a card has no cells:
            // the table, mind-map and kanban arms above are the only things that read a slot
            // as an index, and each is gated on its own kind.
            ItemKind::LinkPreview { .. } | ItemKind::Embed { .. } => {
                CELL_SLOT_BASE.saturating_add(1)
            }
            _ => CELL_SLOT_BASE,
        }
    }

    /// The table's laid-out grid, built at most once per frame per table.
    ///
    /// Keyed on the projection generation *and* the box. The generation alone is not
    /// enough for the same reason the text cache needed fixing: a resize changes every
    /// column, and while `Projection::moved` does bump the generation for a non-
    /// translation, holding the size too makes that a property of this cache rather
    /// than a promise about another module.
    fn table_layout(
        &mut self,
        id: SceneId,
        projected: &Projected,
        generation: u64,
    ) -> &CachedTable {
        let size = projected.item.placement.scaled_size();
        let stale = self
            .tables
            .get(&id)
            .is_none_or(|cached| cached.generation != generation || cached.size != size);
        if stale {
            let ItemKind::Table { model } = &projected.item.kind else {
                unreachable!("only a table is asked for a table layout")
            };
            let table = crate::table::decode(model);
            let layout = crate::table::layout(&table, self.text.engine_mut(), size);
            self.tables.insert(id, CachedTable { generation, size, layout, table });
        }
        &self.tables[&id]
    }

    /// The mind map's laid-out tree, built at most once per frame per map.
    ///
    /// Keyed on the projection generation alone, unlike [`Self::table_layout`]: a tidy
    /// tree's extent comes from the tree and the shaped labels, never from the item's
    /// box, so a resize cannot change it. What a resize changes is the fit scale, and
    /// that is derived from the box at the point of drawing rather than baked in here.
    fn mindmap_layout(
        &mut self,
        id: SceneId,
        projected: &Projected,
        generation: u64,
    ) -> &CachedMindMap {
        let stale = self.mindmaps.get(&id).is_none_or(|cached| cached.generation != generation);
        if stale {
            let ItemKind::MindMap { model } = &projected.item.kind else {
                unreachable!("only a mind map is asked for a mind-map layout")
            };
            let model = crate::mindmap::decode(model);
            let layout = crate::mindmap::layout(&model, self.text.engine_mut());
            let connectors = crate::mindmap::connectors(&model, &layout);
            let natural = crate::mindmap::natural_size(&layout);
            self.mindmaps
                .insert(id, CachedMindMap { generation, layout, model, connectors, natural });
        }
        &self.mindmaps[&id]
    }

    /// The kanban board's laid-out columns, built at most once per frame per board.
    ///
    /// Keyed on the box as well as the generation, like [`Self::table_layout`]: a
    /// column's width comes from the item's width and every card's height is measured at
    /// that width, so a resize invalidates all of it.
    fn kanban_layout(
        &mut self,
        id: SceneId,
        projected: &Projected,
        generation: u64,
    ) -> &CachedKanban {
        let size = projected.item.placement.scaled_size();
        let stale = self
            .kanbans
            .get(&id)
            .is_none_or(|cached| cached.generation != generation || cached.size != size);
        if stale {
            let ItemKind::Kanban { board } = &projected.item.kind else {
                unreachable!("only a kanban is asked for a kanban layout")
            };
            let decoded = crate::kanban::decode(board);
            let (board, layout) = crate::kanban::layout(&decoded, self.text.engine_mut(), size);
            let runs = kanban_runs(&board, &layout);
            self.kanbans.insert(id, CachedKanban { generation, size, layout, runs });
        }
        &self.kanbans[&id]
    }

    /// One kanban label — the board's title, a column's header or a card's — as a block.
    ///
    /// A slot is an index into the flattened [`KanbanRun`] list, so this needs to know
    /// nothing about columns and cards; see that type for why.
    fn kanban_run_block(
        &mut self,
        id: SceneId,
        projected: &Projected,
        slot: u16,
        ctx: &DrawContext<'_>,
    ) -> Option<Painted> {
        let generation = projected.generation;
        let index = usize::from(slot - CELL_SLOT_BASE);
        let placement = projected.item.placement;
        let theme = ctx.theme;

        // Copied out before the engine is borrowed to shape, as the table and mind-map
        // paths do.
        let run = self.kanban_layout(id, projected, generation).runs.get(index)?.clone();
        // While this run is the one being typed into, draw the *field* rather than the
        // decorated label: a column header reads "To do  3/5" and the field behind it is
        // "To do", so the count steps out of the way for as long as the caret is there.
        let editing = ctx.editing.is_some_and(|c| c.scene == id && c.slot == slot);
        let words = if editing { run.field.clone() } else { run.text.clone() };
        // An empty run costs nothing to skip — except when a caret is in it, and then
        // skipping it is what makes the caret invisible: no block, no origin, nothing to
        // draw against. An empty card is exactly what a just-added one is.
        if words.is_empty() && !editing {
            return None;
        }

        let (item_w, item_h) = placement.scaled_size();
        let origin = WorldPoint::new(
            placement.x - item_w / 2.0 + run.rect.left(),
            placement.y - item_h / 2.0 + run.rect.top(),
        );

        let style = Style { font_size: Some(run.font_size), ..Style::default() };
        #[expect(clippy::cast_possible_truncation, reason = "a card's box is screen-scale")]
        let fit = FitBox::new(
            run.rect.width().max(1.0) as f32,
            run.rect.height().max(1.0) as f32,
        );

        if run.font_size * ctx.camera.zoom() < f64::from(MIN_DEVICE_FONT_SIZE) {
            return None;
        }

        let key = BlockKey::new(id, slot);
        let text = vellum_text::StyledText::plain(&words);
        let (_, font_size) = self.text.layout(key, generation, &style, Some(fit), || text.clone());

        let device = ctx.camera.world_to_screen(origin);
        Some(Painted::Glyphs(Block {
            origin: ScreenPoint::new(device.x.round(), device.y.round()),
            font_size,
            color: if run.muted { theme.text_muted } else { theme.text },
            // A widget's internals draw at the camera's own zoom. Only a frame's title
            // floors its magnification — see `Block::scale`.
            scale: ctx.camera.zoom() as f32,
        }))
    }

    /// An Agent Canvas node's pieces, built at most once per frame per node.
    ///
    /// # Why this cache is keyed on a frame counter and nothing else is
    ///
    /// Every other layout cache here compares [`Projected::generation`], because everything
    /// else a widget draws *is* in the document and a change to it moves that stamp. An
    /// agent's transcript is not: `docs/07` §4 keeps it in a disposable sidecar precisely so
    /// that agent output never enters the Loro document, and a note's body is a `.md` file on
    /// disk. So the document is silent when the thing on screen changes, and a
    /// generation-keyed cache would draw the previous sentence forever.
    ///
    /// One rebuild per visible node per frame is the honest cost of that, and it is bounded
    /// by the viewport rather than by the board — an off-screen node is never asked, which is
    /// `docs/07` §5d's rule that culling stops the *drawing* and never the process.
    fn node_paint(
        &mut self,
        id: SceneId,
        projected: &Projected,
        ctx: &DrawContext<'_>,
    ) -> &NodePaint {
        let size = projected.item.placement.scaled_size();
        let generation = projected.generation;
        let frame = self.frame;
        let signature = node_signature(&projected.item.kind, ctx.agents.get(id));
        let stale = self.nodes.get(&id).is_none_or(|cached| {
            cached.frame != frame
                || cached.generation != generation
                || cached.size != size
                || cached.signature != signature
        });
        if stale {
            let paint =
                build_node_paint(&projected.item.kind, ctx.agents.get(id), ctx, id, size);
            self.nodes.insert(id, CachedNode { frame, generation, size, signature, paint });
        }
        &self.nodes[&id].paint
    }

    /// One run of an Agent Canvas node's text, as a block.
    ///
    /// A slot is an index into the node's flattened run list, so this knows nothing about
    /// transcripts, tree rows or option cards — the same division [`KanbanRun`] makes, and
    /// for the same reason: two flattenings of the same list is a caret that appears on one
    /// card and types into its neighbour.
    fn node_run_block(
        &mut self,
        id: SceneId,
        projected: &Projected,
        slot: u16,
        ctx: &DrawContext<'_>,
    ) -> Option<Painted> {
        let index = usize::from(slot - CELL_SLOT_BASE);
        let placement = projected.item.placement;
        let theme = ctx.theme;

        // Copied out before the engine is borrowed to shape, as every other indexed path
        // here does: the run list lives in `self.nodes` and the shaping needs `self.text`.
        let (run, has_caret) = {
            let paint = self.node_paint(id, projected, ctx);
            (paint.run(index)?.clone(), paint.prompt_slot() == Some(slot))
        };
        // **An empty run still gets a block when the caret is in it**, which is feedback 25's
        // rule — `Painter::block` skipping a wordless slot is right for a board of blank notes
        // and catastrophic while a caret is in one, because no block means no origin and
        // `push_caret` has nothing to measure from. That fix was applied to a table cell and a
        // kanban card and to nothing else; this is the fourth field it turns out to need.
        if run.text.trim().is_empty() && !has_caret {
            return None;
        }

        // A run's rectangle is in the item's own space with the item's top-left at the
        // origin, so the world position is one addition — the table's and the kanban's rule.
        let (item_w, item_h) = placement.scaled_size();
        let origin = WorldPoint::new(
            placement.x - item_w / 2.0 + run.rect.x,
            placement.y - item_h / 2.0 + run.rect.y,
        );

        let style = Style {
            font_size: Some(run.font_size),
            line_height: Some(NODE_LINE_HEIGHT),
            align: Some(run.align),
            ..Style::default()
        };
        let fit = FitBox::new(run.rect.width.max(1.0) as f32, run.rect.height.max(1.0) as f32);
        let colour = tone_colour(run.tone, &theme);
        let zoom = ctx.camera.zoom();

        // **Greeked rather than dropped, and greeked per run.** A kanban card returns `None`
        // below the threshold, which is right for a handful of short labels on a coloured
        // box; a transcript is nothing *but* text, so a node that dropped it would be an
        // empty rectangle on a zoomed-out board — the exact failure `Painted::Greeked` was
        // added for, since `ItemKind::Text` has no geometry to fall back on either. One bar
        // per run is what a stack of short paragraphs looks like from far away, and the
        // count is bounded by the run list, which is bounded by the node's own box.
        if run.font_size * zoom < f64::from(MIN_DEVICE_FONT_SIZE) {
            return Some(Painted::Greeked(Greek {
                origin,
                column: run.rect.width,
                align: run.align,
                color: colour,
                lines: GreekLines::Single {
                    width: run.rect.width,
                    height: greek_bar_height(run.font_size * NODE_LINE_HEIGHT, zoom),
                },
            }));
        }

        let key = BlockKey::new(id, slot);
        // **The run's own stamp, not the projection's generation** — see [`NodeRun::stamp`].
        // Mixed with the generation so that moving or resizing the item still invalidates,
        // which the content hash alone would not.
        let stamp = run.stamp ^ projected.generation;
        let text = vellum_text::StyledText::plain(&run.text);
        let (_, font_size) = self.text.layout(key, stamp, &style, Some(fit), || text.clone());

        let device = ctx.camera.world_to_screen(origin);
        Some(Painted::Glyphs(Block {
            origin: ScreenPoint::new(device.x.round(), device.y.round()),
            font_size,
            color: colour,
            // A widget's internals draw at the camera's own zoom. Only a frame's title
            // floors its magnification — see `Block::scale`.
            scale: ctx.camera.zoom() as f32,
        }))
    }

    /// One mind-map node's label, as a block.
    ///
    /// Positioned from the laid-out tree rather than from the item's own box, exactly as
    /// a table cell is: where a node sits is what the tidy pass computed, and nothing
    /// about the placement alone can say.
    ///
    /// The label is inset by the same padding the node's box was measured with, so it
    /// lands where the measurement said it would rather than being centred by a second,
    /// disagreeing rule.
    fn mindmap_node_block(
        &mut self,
        id: SceneId,
        projected: &Projected,
        slot: u16,
        ctx: &DrawContext<'_>,
    ) -> Option<Painted> {
        let generation = projected.generation;
        let index = usize::from(slot - CELL_SLOT_BASE);
        let placement = projected.item.placement;
        let theme = ctx.theme;

        // Copied out of the cache before the engine is borrowed to shape, as the table
        // path does: the layout lives in `self.mindmaps` and shaping needs `self.text`.
        let node = {
            let cached = self.mindmap_layout(id, projected, generation);
            let scale = cached.scale(placement.scaled_size());
            let node = cached.layout.placements().get(index)?;
            let rect = node.rect;
            let model = cached.model.map.get(node.node)?;
            // As with a table cell: empty nodes are skipped, except the one holding a caret.
            if model.text.is_empty()
                && !ctx.editing.is_some_and(|c| c.scene == id && c.slot == slot)
            {
                return None;
            }
            (rect, model.style, crate::mindmap::label(&model.text, &model.style), scale)
        };
        let (rect, style, content, scale) = node;

        // The map's rectangles start at the map's own top-left, which the fit scale then
        // maps onto the item's box.
        let (item_w, item_h) = placement.scaled_size();
        let pad = (
            crate::mindmap::NODE_PADDING_X * scale,
            crate::mindmap::NODE_PADDING_Y * scale,
        );
        let origin = WorldPoint::new(
            placement.x - item_w / 2.0 + rect.min.x * scale + pad.0,
            placement.y - item_h / 2.0 + rect.min.y * scale + pad.1,
        );

        // Bold is not on `Style` — it rides on the span, which `crate::mindmap::label`
        // has already folded the node's flag into, so it is not restated here.
        let block_style = Style {
            font_size: Some(style.font_size * scale),
            align: Some(Align::Center),
            ..Style::default()
        };
        let colour = Rgba::from_rgb8(style.text.r, style.text.g, style.text.b)
            .with_alpha(f32::from(style.text.a) / 255.0);
        let colour = if style.text.is_visible() { colour } else { theme.text };
        #[expect(clippy::cast_possible_truncation, reason = "a node's box is screen-scale")]
        let fit = FitBox::new(
            (rect.width() * scale - pad.0 * 2.0).max(1.0) as f32,
            (rect.height() * scale - pad.1 * 2.0).max(1.0) as f32,
        );

        // Nothing readable to draw. Checked here as well as in `block`'s shared bar
        // because that bar is reached only by the named slots above it.
        if style.font_size * scale * ctx.camera.zoom() < f64::from(MIN_DEVICE_FONT_SIZE) {
            return None;
        }

        let key = BlockKey::new(id, slot);
        let (_, font_size) =
            self.text.layout(key, generation, &block_style, Some(fit), || content.clone());

        let device = ctx.camera.world_to_screen(origin);
        Some(Painted::Glyphs(Block {
            origin: ScreenPoint::new(device.x.round(), device.y.round()),
            font_size,
            color: colour,
            // A widget's internals draw at the camera's own zoom. Only a frame's title
            // floors its magnification — see `Block::scale`.
            scale: ctx.camera.zoom() as f32,
        }))
    }

    /// Projects a text block's world rectangle onto the device pixel grid.
    fn locate(&self, ctx: &DrawContext<'_>, request: BlockRequest<'_>) -> Block {
        let font_size = request.font_size;
        let color = request.color;
        let scale = request.scale;
        let device = ctx.camera.world_to_screen(Self::locate_world(request));
        Block {
            // Snapping to whole device pixels keeps a glyph's subpixel phase stable
            // between frames, so text does not shimmer while the camera is still.
            origin: ScreenPoint::new(device.x.round(), device.y.round()),
            font_size,
            color,
            scale,
        }
    }

    /// A text block's top-left corner, in world coordinates.
    ///
    /// Split out of [`Self::locate`] because greeked bars are *board*-view quads and
    /// need the world point, where glyphs are screen-view and need the device one.
    /// Sharing it means the four anchoring rules are written once — a bar sits exactly
    /// where the text it replaces would have sat.
    fn locate_world(request: BlockRequest<'_>) -> WorldPoint {
        let BlockRequest {
            placement,
            block_width,
            block_height,
            anchor,
            font_size: _,
            color: _,
            // The world point is unaffected by how large the glyphs are drawn: the anchor
            // already carries the magnified offset — see `Painter::block`, which scales
            // `Anchor::Above` before building the request.
            scale: _,
        } = request;
        let (width, height) = placement.scaled_size();
        let left = placement.x - block_width / 2.0;
        let top = match anchor {
            Anchor::Centred => placement.y - block_height / 2.0,
            Anchor::TopLeft => placement.y - height / 2.0,
            // A frame's name sits above its top edge, clear of the frame's fill.
            Anchor::Above(size) => placement.y - height / 2.0 - size * 1.4,
            Anchor::Inset(_, dy) => placement.y - height / 2.0 + dy,
        };
        let world_left = match anchor {
            Anchor::TopLeft | Anchor::Above(_) => placement.x - width / 2.0,
            Anchor::Inset(dx, _) => placement.x - width / 2.0 + dx,
            Anchor::Centred => left,
        };

        WorldPoint::new(world_left, top)
    }

    /// Draws a ring around every selected item.
    fn push_selection(&self, list: &mut DrawList, ctx: &DrawContext<'_>, board: u32) {
        if ctx.selection.is_empty() {
            return;
        }
        list.use_view(board);
        // A selection ring is chrome: constant on screen, so its world width has to
        // shrink as the board is zoomed in. Its own constant, thinner than the handles
        // and the gesture chrome — see `SELECTION_RING_WIDTH`.
        let width = (f64::from(SELECTION_RING_WIDTH) / ctx.camera.zoom()) as f32;
        for id in ctx.selection {
            let Some(projected) = ctx.projection.get(*id) else { continue };
            let (origin, (w, h)) = projected.rect();
            list.push_quad(
                QuadInstance::solid(
                    ctx.camera.to_camera_relative(origin),
                    [w as f32, h as f32],
                    Rgba::TRANSPARENT,
                )
                .with_border(ctx.theme.accent, width)
                .with_corner_radius(STICKY_RADIUS)
                .with_rotation(projected.rotation()),
            );
        }
        push_handles(list, ctx, board);
    }
}

/// Miro's four blue dots, on whichever item is wearing them this frame.
///
/// *"in miro there are these 4 blue dots around the picture … and when i hold and draw i
/// should be able to connect it to other agents sticky notes or agents or pictures."*
///
/// Drawn in the **board** view for `push_handles`' reason: the ports sit on the item as it
/// is drawn, including when it is turned, and every size is divided by the zoom so they stay
/// constant on screen. A screen-space dot would be visibly off the edge it names on any
/// rotated item.
///
/// **Round, filled with the accent, ringed in the surface colour** — the inverse of a resize
/// handle, which is surface filled and accent ringed. That inversion is the whole
/// distinction: at a glance the four dots are *solid* and the eight handles are *hollow*, so
/// which grip is under the pointer is answerable without aiming at it. A filled dot also
/// survives being drawn over a dark image, where a hollow one disappears into it.
///
/// Not gated on the selection: [`DrawContext::ports`] is already the app's whole answer to
/// *"which item, if any"*, and re-deciding here would be the second copy of a rule that
/// `card_layout` exists to warn about.
fn push_ports(list: &mut DrawList, ctx: &DrawContext<'_>, board: u32) {
    let Some(id) = ctx.ports else { return };
    let Some(projected) = ctx.projection.get(id) else { return };
    let zoom = ctx.camera.zoom();
    let radius = f64::from(crate::handle::PORT_RADIUS) / zoom;
    let ring = (SELECTION_WIDTH as f64 / zoom) as f32;

    list.use_view(board);
    for (_, at) in crate::handle::ports(&projected.item.placement, zoom) {
        let origin = WorldPoint::new(at.x - radius, at.y - radius);
        let side = (radius * 2.0) as f32;
        list.push_quad(
            QuadInstance::solid(
                ctx.camera.to_camera_relative(origin),
                [side, side],
                ctx.theme.accent,
            )
            // A circle, drawn as a quad whose corner radius is its own half-width — the
            // same construction the rotate handle uses, so there is one way of making a
            // round grip in this file rather than two.
            .with_corner_radius(side / 2.0)
            // Against the item's own fill, which is what a dot sitting half off the edge of
            // a dark image needs to stay a dot. It is **not** rotated: a circle has no
            // orientation, and passing the item's rotation would only cost the vertex
            // shader work that changes nothing.
            .with_border(ctx.theme.surface, ring),
        );
    }
}

/// The resize and rotate handles, on a single selection.
///
/// Only one item, matching what `crate::actions` will actually grab: handles on a
/// multi-selection would have to resize every member against a shared box and move each
/// centre as well as its angle, which is a different operation rather than a bigger
/// version of this one. A multi-selection keeps its ring and still drags.
///
/// Drawn in the **board** view so they sit on the item as it is drawn — including when
/// it is rotated, where a screen-space handle would be visibly off the corner it names
/// — with every size divided by the zoom so they stay constant on screen. That is the
/// same trade the selection ring makes directly above.
fn push_handles(list: &mut DrawList, ctx: &DrawContext<'_>, board: u32) {
    let zoom = ctx.camera.zoom();
    let size = f64::from(crate::handle::HANDLE_SIZE) / zoom;
    let border = (SELECTION_WIDTH as f64 / zoom) as f32;

    // More than one item: the handles belong to the *group's* box, which is axis-aligned and
    // offers corners and rotate only. See `handle::GROUP_HANDLES` for why there are no edge
    // handles on a group — an edge drag shears a rotated member, and no `Placement` is a
    // sheared rectangle.
    if ctx.selection.len() > 1 {
        let placements: Vec<vellum_doc::Placement> = ctx
            .selection
            .iter()
            .filter_map(|id| ctx.projection.get(*id))
            .map(|projected| projected.item.placement)
            .collect();
        let Some(group) = crate::handle::group_bounds(&placements) else { return };
        list.use_view(board);
        // The group's own outline, so it is visible what is about to be transformed. Each
        // member already draws its own ring; this is the box the handles act on, which is
        // not the same thing and is otherwise invisible.
        push_group_outline(list, ctx, &group);
        for (handle, at) in crate::handle::group_positions(&group, zoom) {
            let origin = WorldPoint::new(at.x - size / 2.0, at.y - size / 2.0);
            list.push_quad(
                QuadInstance::solid(
                    ctx.camera.to_camera_relative(origin),
                    [size as f32, size as f32],
                    ctx.theme.surface,
                )
                .with_border(ctx.theme.accent, border)
                .with_corner_radius(if handle.is_rotate() { (size / 2.0) as f32 } else { 0.0 }),
            );
        }
        return;
    }

    // A connector gets its **two ends** instead, and nothing else. Its box is derived from
    // its bindings rather than chosen (`connector::placement_for`), so eight resize handles
    // on it are eight grips that move nothing a user can see — and the rotate grip is worse,
    // since rotating a rectangle that is only there to normalise free ends turns the line
    // without turning what it joins. Miro gives a line two round grips; so does this.
    if let Some((from, to)) = ctx.connector_grips {
        list.use_view(board);
        let radius = f64::from(crate::handle::PORT_RADIUS) / zoom;
        for (x, y) in [from, to] {
            let origin = WorldPoint::new(x - radius, y - radius);
            let side = (radius * 2.0) as f32;
            list.push_quad(
                QuadInstance::solid(
                    ctx.camera.to_camera_relative(origin),
                    [side, side],
                    ctx.theme.accent,
                )
                .with_corner_radius(side / 2.0)
                .with_border(ctx.theme.surface, border),
            );
        }
        return;
    }

    let [id] = ctx.selection[..] else { return };
    let Some(projected) = ctx.projection.get(id) else { return };
    let placement = projected.item.placement;
    list.use_view(board);

    for (handle, at) in crate::handle::positions(&placement, zoom) {
        let origin = WorldPoint::new(at.x - size / 2.0, at.y - size / 2.0);
        let quad = QuadInstance::solid(
            ctx.camera.to_camera_relative(origin),
            [size as f32, size as f32],
            ctx.theme.surface,
        )
        .with_border(ctx.theme.accent, border)
        // Square handles, per `docs/05-design-language.md` §4 — "square handles, no
        // glow" — except the rotate one, which is round so that it reads as a
        // different verb before it is touched rather than after.
        .with_corner_radius(if handle.is_rotate() { (size / 2.0) as f32 } else { 0.0 })
        .with_rotation(projected.rotation());
        list.push_quad(quad);
    }
}

/// The multi-selection's shared box, as four hairlines.
///
/// Four edges rather than one bordered quad because a filled quad — even a transparent one —
/// would sit over the items inside it and take their clicks in the hit-test the *painter*
/// does not do but the debug reader assumes. Hairlines are also what the design language
/// asks for: one line does the work.
fn push_group_outline(list: &mut DrawList, ctx: &DrawContext<'_>, group: &crate::handle::Group) {
    let zoom = ctx.camera.zoom();
    let width = (f64::from(SELECTION_WIDTH) / zoom) as f32;
    let (left, top) = (group.x - group.width / 2.0, group.y - group.height / 2.0);
    let corners = [
        (left, top, group.width, 0.0),
        (left, top + group.height, group.width, 0.0),
        (left, top, 0.0, group.height),
        (left + group.width, top, 0.0, group.height),
    ];
    for (x, y, w, h) in corners {
        let origin = ctx.camera.to_camera_relative(WorldPoint::new(x, y));
        list.push_quad(QuadInstance::solid(
            origin,
            [(w as f32).max(width), (h as f32).max(width)],
            // Muted against the members' own accent rings, so the group box reads as the
            // frame around them rather than as a sixth selected thing.
            ctx.theme.accent.with_alpha(0.45),
        ));
    }
}

/// A chart's grid hairline, in world units before the zoom divides it.
const GRID_WIDTH: f32 = 1.0;

/// The thinnest a mind map's branch is drawn, in world units.
///
/// A fit scale below about 0.3 would otherwise take a 2px branch under half a world
/// unit, and a map shrunk into a small box would lose its lines before it lost its
/// boxes — which reads as broken rather than as small.
const MIN_BRANCH_WIDTH: f32 = 0.75;

/// One axis rule or baseline, as a quad.
///
/// Chart segments are always axis-aligned — a grid line, a zero rule, an axis spine —
/// so a quad is exact rather than an approximation, and cheaper than a mesh.
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
/// The vertices start at the item's top-left — which is how both a chart's marks and a
/// mind map's branches are laid out — so the transform is that corner and the mesh
/// needs no per-vertex offset.
fn push_local_mesh(
    list: &mut DrawList,
    position: [f32; 2],
    mesh: &vellum_shapes::Mesh,
    colour: Rgba,
    rotation: f32,
) {
    if mesh.indices.is_empty() {
        return;
    }
    let transform = list.meshes_mut().push_transform(MeshTransform::scale_rotate_at(
        1.0,
        rotation,
        position,
    ));
    let start = list.meshes().indices().len() as u32;
    list.meshes_mut().push_shape_fill(mesh, colour, transform);
    let end = list.meshes().indices().len() as u32;
    list.push_meshes(start..end);
}

/// Every label a kanban board draws, flattened in the order slots are handed out:
/// the board's title, then each column's header, then that column's cards.
///
/// Built with the layout rather than on demand so that a slot index is stable for a
/// frame, and so the *count* is known without a second walk — `slots_of` needs it.
pub(crate) fn kanban_runs(
    board: &vellum_flow::Kanban,
    laid: &vellum_flow::KanbanLayout,
) -> Vec<KanbanRun> {
    use vellum_flow::Rect as FlowRect;
    let pad = crate::kanban::CARD_PADDING;
    let mut runs = Vec::with_capacity(1 + laid.columns.len() + board.card_count());

    runs.push(KanbanRun {
        rect: laid.title.inset_by(pad),
        text: board.title().to_owned(),
        part: crate::edit::EditPart::KanbanTitle,
        field: board.title().to_owned(),
        font_size: crate::kanban::TITLE_FONT_SIZE,
        muted: false,
    });

    for column in &laid.columns {
        let Some(model) = board.column(column.id) else { continue };
        // The header carries the count — and the limit when there is one, which is what
        // makes a WIP limit visible rather than merely enforced. `vellum-flow` computes
        // the numbers; the wording is the only thing decided here.
        let wip = column.wip;
        let label = match wip.limit {
            Some(limit) => format!("{}  {}/{}", model.title(), wip.count, limit),
            None => format!("{}  {}", model.title(), wip.count),
        };
        runs.push(KanbanRun {
            rect: column.header.inset_by(pad * 0.5),
            text: label,
            part: crate::edit::EditPart::KanbanColumn(column.id),
            field: model.title().to_owned(),
            font_size: crate::kanban::HEADER_FONT_SIZE,
            muted: true,
        });

        for card in &column.cards {
            let Some(model) = board.card(card.id) else { continue };
            runs.push(KanbanRun {
                // Insetting rather than using the rect keeps the label off the card's
                // rounded corner. The same padding the height was measured with, so the
                // text fits the box that was sized for it.
                rect: FlowRect::new(
                    card.rect.left() + pad,
                    card.rect.top() + pad,
                    (card.rect.width() - pad * 2.0).max(1.0),
                    (card.rect.height() - pad * 2.0).max(1.0),
                ),
                text: model.label().to_owned(),
                part: crate::edit::EditPart::KanbanCard(card.id),
                field: model.label().to_owned(),
                font_size: crate::kanban::CARD_FONT_SIZE,
                muted: false,
            });
        }
    }
    runs
}

/// A `vellum-table` colour as the renderer's.
///
/// Its own type rather than a shared one because `vellum-table` has no renderer
/// dependency — it is a layout crate, and `docs/01-architecture.md` keeps it that way.
fn table_colour(c: vellum_table::Rgba) -> Rgba {
    Rgba::from_rgb8(c.r, c.g, c.b).with_alpha(f32::from(c.a) / 255.0)
}

/// The largest font size a block could possibly be shaped at, without shaping it.
///
/// An explicit size is itself; an auto-fitted one is bounded by the height of the box
/// it has to fit inside, because a single line is at least as tall as its font size.
/// Conservative in the safe direction: it can only over-estimate, so nothing readable
/// is ever skipped.
fn largest_font_size(style: &Style, fit: FitBox) -> f64 {
    match style.font_size {
        Some(size) if size.is_finite() && size > 0.0 => size,
        _ => f64::from(fit.height),
    }
}

/// A laid-out block and where on its item it belongs, on the way to being placed.
struct BlockRequest<'a> {
    placement: &'a vellum_doc::Placement,
    /// The width the block was laid out against — its wrap width, not its extent,
    /// because alignment is already baked into the glyph positions.
    block_width: f64,
    block_height: f64,
    anchor: Anchor,
    font_size: f32,
    color: Rgba,
    /// See [`Block::scale`]. The camera's zoom for everything but a frame's title.
    scale: f32,
}

/// Where a text block hangs off its item.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Anchor {
    /// Centred in the item, both ways. A sticky.
    Centred,
    /// The item's own top-left corner. A text widget.
    TopLeft,
    /// Above the item's top edge, by the given font size. A frame's name.
    Above(f64),
    /// An explicit offset from the item's top-left, in world units — `(dx, dy)`.
    ///
    /// For a link card, whose four pieces are positioned by [`card_layout`] rather than by a
    /// named rule. A named anchor per piece would be four rules that have to agree.
    Inset(f64, f64),
}

/// A laid-out block, placed.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Block {
    /// Device pixels, snapped to the grid.
    origin: ScreenPoint,
    /// World-pixel size the block was shaped at.
    font_size: f32,
    color: Rgba,
    /// Device pixels per world unit for *these* glyphs — normally the camera's zoom.
    ///
    /// **Its own field because a frame's title refuses to shrink past legibility.** The naive
    /// way to floor a world-sized label is to raise `font_size` as the camera pulls back, and
    /// that silently does nothing: `TextCache::layout` keys staleness on `(BlockKey,
    /// generation)` alone, zooming bumps neither, so the cache keeps handing back the layout
    /// shaped at the old size *and* the old `font_size` — which is the number the greek guard
    /// then tests. Measured before writing the fix; it looked like the fix had no effect.
    ///
    /// `DrawList::push_layout` applies its `scale` purely as glyph magnification and transforms
    /// the origin separately, which is the seam `territory_label` already uses from the other
    /// end — it passes `1.0` in the screen view. So the layout is shaped once at a world size
    /// and simply drawn larger. No cache key changes, nothing re-shapes, and a zoom gesture
    /// costs exactly what it did.
    scale: f32,
}

/// What one of an item's text slots contributes to the frame.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Painted {
    /// Shaped, and big enough to read: real glyphs.
    Glyphs(Block),
    /// Real text, too small to read: bars standing in for it.
    ///
    /// Dropping it instead — which is what happened before this existed — leaves a
    /// zoomed-out board looking empty where it is full, and a `ItemKind::Text` has no
    /// geometry of its own to fall back on, so it disappeared completely.
    Greeked(Greek),
}

/// A text block reduced to bars, in world coordinates.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Greek {
    /// Top-left of the text column. World coordinates, before the camera rebase.
    origin: WorldPoint,
    /// The column the text was wrapped into — what alignment is measured against.
    column: f64,
    align: Align,
    /// The colour the text itself would have used, before [`GREEK_ALPHA`].
    color: Rgba,
    lines: GreekLines,
}

/// How a greeked block's bars are laid out.
#[derive(Debug, Clone, Copy, PartialEq)]
enum GreekLines {
    /// One bar for the whole block: either nothing was shaped, or the lines are too
    /// close together to tell apart.
    Single { width: f64, height: f64 },
    /// One bar per laid-out line, read back from the cached layout at push time.
    ///
    /// Carries the key rather than the lines so that a greeked block allocates
    /// nothing — the layout it names is already resident and already paid for.
    PerLine { key: BlockKey, bar: f64 },
    /// A stack of bars for a block that was **never shaped**, sized from the type rather
    /// than from a layout.
    ///
    /// # Why this variant has to exist
    ///
    /// [`Painter::block`]'s pre-shape guard bails out before any layout exists, and it used
    /// to emit `Single` sized from `fit.height` — the whole text box. That is right for an
    /// *auto-fitted* block, where `largest_font_size` **is** `fit.height` and the box really
    /// is one line tall. It is wrong for a block with an **explicit** size, where the guard
    /// tests 13 units and the bar is drawn from the ~142-unit body: one grey slab a third of
    /// the card tall, which is what the user photographed and described as having to zoom in
    /// to see any titles at all. A sticky never showed it, because a sticky auto-fits and so
    /// takes the post-shape path into `PerLine`.
    ///
    /// Estimated rather than measured, and that is the point: shaping is exactly what the
    /// guard exists to avoid on a board where every block is sub-pixel. The line count comes
    /// from the box and the line height, which is an upper bound on what could be in there —
    /// at this size the difference between the bound and the truth is well under a pixel.
    Estimated { lines: usize, bar: f64, spacing: f64, width: f64 },
}

/// The most bars an unshaped block will stand in for.
///
/// A bound rather than a budget: the line count is estimated from the box, and a box can be
/// dragged to any height at all, so without this a 100,000-unit frame at a low zoom would ask
/// for thousands of quads to represent text nobody can read. Twelve is past what any card or
/// sticky on the reference board holds, and a stack that long already reads as "a paragraph".
const MAX_GREEK_LINES: f64 = 12.0;

/// How tall a bar standing in for a line of `line_height` world pixels should be.
///
/// The fraction alone goes sub-pixel exactly where greeking starts mattering, so it
/// carries a floor of one *device* pixel — a screen-space budget divided by the zoom,
/// the same shape as `vellum_ink::Lod::world_tolerance` and the selection ring, and
/// guarded against a nonsense zoom the same way.
fn greek_bar_height(line_height: f64, zoom: f64) -> f64 {
    let floor = if zoom.is_finite() && zoom > 0.0 {
        GREEK_MIN_DEVICE_HEIGHT / zoom
    } else {
        GREEK_MIN_DEVICE_HEIGHT
    };
    (line_height * GREEK_BAR_FRACTION).max(floor)
}

/// Fraction of a card's height taken by its thumbnail.
// `CARD_FONT_SIZE`, `LINK_CARD_REFERENCE_WIDTH` and the function over them are
// `vellum_project::card`'s, shared with the browser painter — which had its own, unrelated,
// derived from the card's *height*, and drew every card's type 69% larger as a result.
/// A card's text size at its own width, so the painter and `block` agree on one number.
pub(crate) fn card_font_size(width: f64) -> f64 {
    vellum_project::card::base_font_size(width)
}

/// A favicon's corner rounding, in device pixels. Small: an icon is nearly square and a
/// generous radius turns a logo into a blob.
const FAVICON_RADIUS: f32 = 2.0;

/// Paints the open-page badge: a round plate and a `↗` on it.
///
/// # Why the arrow is geometry rather than a character
///
/// `↗` is U+2197, which lives in *Noto Sans Symbols* rather than in Noto Sans — and Noto
/// Whether a card's page is a video, so its poster gets a ▶.
pub(crate) fn is_video(kind: &ItemKind) -> bool {
    link_url(kind).is_some_and(vellum_link::plays_video)
}

/// The ▶ over a video card's poster frame.
///
/// # Three quads, and a triangle drawn as a fan of them
///
/// The same reasoning as `push_open_badge`'s arrow, arrived at for a harder glyph: `▶` is
/// U+25B6, which lives outside the plain sans faces (trap 10), so a card asked to shape it
/// draws tofu wherever the fallback chain misses a symbols face. A triangle has no such
/// problem — it is an outline, and this pass already pushes tessellated meshes for ink.
///
/// Drawn as a **mesh**, unlike the badge's arrow: an arrow is three bars and a triangle is
/// not expressible as axis-aligned rectangles at all. It goes through the same `MeshBatch`
/// that carries ink, which as of this round is multisampled — so its diagonals are smooth
/// rather than stepped, which a triangle at this size would otherwise show badly.
///
/// The plate is deliberately **dark and semi-transparent** rather than the accent: it sits on
/// an arbitrary photograph, and every video player in the world draws this mark that way, so
/// it is the one place where following the convention beats following the palette.
fn push_play_button(
    list: &mut DrawList,
    (at, extent): ([f32; 2], [f32; 2]),
    rotation: f32,
    opacity: f32,
    theme: &crate::theme::Theme,
    hovered: bool,
) {
    let side = extent[0].min(extent[1]);
    if side <= 0.0 {
        return;
    }
    // Hover fills the plate with the accent, matching what the ↗ badge does — the two are the
    // only buttons on the board and they must not answer a pointer differently.
    let plate = if hovered { theme.accent } else { PLAY_PLATE };
    list.push_quad(
        QuadInstance::solid(at, extent, plate.with_alpha(plate.a * opacity))
            .with_corner_radius(side * 0.5)
            .with_rotation(rotation),
    );

    // The triangle, inset and nudged right so it sits optically centred: a triangle centred on
    // its bounding box reads as left-of-centre, because its mass is toward the flat edge.
    let centre = [at[0] + extent[0] * 0.5, at[1] + extent[1] * 0.5];
    #[expect(clippy::cast_possible_truncation, reason = "a glyph is screen-scale")]
    let reach = (f64::from(side) * 0.5 * PLAY_GLYPH) as f32;
    let nudge = reach * 0.18;
    // **Relative to the centre, because the transform below translates by it.** These were
    // absolute, so `scale_rotate_at`'s translation added the centre a *second* time and flung
    // the triangle to twice its own offset — the user photographed it as a stray play mark in
    // the empty board above the card, with the plate sitting correctly on the poster. The ink
    // path has always had this right: `vellum-ink` stores its points relative to the item's
    // centre and passes that centre as the translation.
    let tip = [reach + nudge, 0.0];
    let top = [-reach + nudge, -reach];
    let bottom = [-reach + nudge, reach];
    let ink = theme.surface.with_alpha(opacity);
    // **Rotated about the card's own centre**, like the plate above it. An identity transform
    // left the triangle upright inside a plate that turned, so a rotated video card drew a
    // play button lying on its side inside an upright ring — which reads as a rendering fault
    // rather than as a rotated card. `scale_rotate_at` takes the centre it turns about, and
    // for a circle that is its own middle.
    let transform = list
        .meshes_mut()
        .push_transform(MeshTransform::scale_rotate_at(1.0, rotation, centre));
    let start = list.meshes().indices().len() as u32;
    list.meshes_mut().push_indexed(&[top, tip, bottom], &[0, 1, 2], ink, transform);
    let end = list.meshes().indices().len() as u32;
    list.push_meshes(start..end);
}

/// The play button's plate. Charcoal at 62%, the convention every video player uses.
const PLAY_PLATE: vellum_render::Rgba = vellum_render::Rgba::new(0.06, 0.07, 0.08, 0.62);

/// How much of the plate the triangle spans.
const PLAY_GLYPH: f64 = 0.44;

/// A tile where a site's icon should be, when its icon is not there to draw.
///
/// # Why a placeholder rather than nothing
///
/// `card_layout` reserves the favicon's box whenever the item carries a favicon *hash*, and
/// a hash can outlive its picture in three ordinary ways: the fetch has not landed yet, the
/// bytes were an encoding the decoder does not hold, or the texture has been evicted under
/// the residency budget. In all three the row was indented around an empty square, which the
/// user photographed on an Alibaba card and read — correctly — as the card being broken.
///
/// A muted tile at the favicon's own radius says "a site icon belongs here" without claiming
/// to be one. Deliberately **not** a letter or a globe: both need a glyph, and this pass
/// pushes quads (see `push_open_badge` for the same reasoning about `↗`, and `CLAUDE.md`
/// trap 10 for what asking for a character nothing has bundled actually draws).
///
/// It is drawn faintly rather than in the border colour at full strength, so a board of
/// unfetched cards reads as quiet rather than as a grid of grey chips.
fn push_favicon_placeholder(
    list: &mut DrawList,
    at: [f32; 2],
    extent: [f32; 2],
    rotation: f32,
    opacity: f32,
    theme: &crate::theme::Theme,
) {
    if extent[0] <= 0.0 || extent[1] <= 0.0 {
        return;
    }
    list.push_quad(
        QuadInstance::solid(at, extent, theme.border)
            .with_corner_radius(FAVICON_RADIUS)
            .with_rotation(rotation)
            .with_opacity(opacity * FAVICON_PLACEHOLDER_ALPHA),
    );
}

/// How present the stand-in favicon is. Enough to hold the space, quiet enough not to be
/// mistaken for an icon that failed to load into a box that was meant to be dark.
const FAVICON_PLACEHOLDER_ALPHA: f32 = 0.7;

/// What a link card's picture band draws before its pixels arrive.
///
/// The band is by far the largest box on a card, so this is the difference between a card
/// that reads as *loading* and one that reads as broken — *"i just see a box"*.
///
/// **Quieter than the favicon's stand-in, deliberately.** That one fills a 12-point square
/// and needs to be seen; this fills most of a card, and at the favicon's weight a wall of
/// unfetched cards would read as a grid of grey slabs — louder than the pictures it is
/// standing in for. It carries the picture's own corner radius rather than the card's, so
/// the placeholder sits exactly where the image will, and a card whose image arrives
/// mid-session does not appear to change shape.
fn push_card_image_placeholder(
    list: &mut DrawList,
    at: [f32; 2],
    extent: [f32; 2],
    rotation: f32,
    opacity: f32,
    theme: &crate::theme::Theme,
) {
    if extent[0] <= 0.0 || extent[1] <= 0.0 {
        return;
    }
    list.push_quad(
        QuadInstance::solid(at, extent, theme.border)
            .with_corner_radius(CARD_RADIUS * 0.75)
            .with_rotation(rotation)
            .with_opacity(opacity * CARD_IMAGE_PLACEHOLDER_ALPHA),
    );
}

/// See [`push_card_image_placeholder`]. A third of the favicon's weight, because it covers
/// something like forty times the area.
const CARD_IMAGE_PLACEHOLDER_ALPHA: f32 = 0.25;

/// The **mark is geometry, not a character**, and that is not negotiable: U+2197 and U+29C9
/// both live outside every face bundled with this build, and `font_family: None` resolves
/// through fontdb's `sans-serif` alias to a face that has neither (`CLAUDE.md` trap 10) — so a
/// card asked to shape one draws a tofu box on the one control whose whole job is to look
/// pressable. Seven quads always draw.
///
/// *"make the outgoing link more like miro as well please."* Miro's is a thin dark outline of
/// a box with an arrow leaving through its missing top-right corner, on a rounded-square white
/// plate. Velm's was a heavy accent-coloured `↗` on a disc — which on the user's own build,
/// where the accent is the blue, made it the loudest thing on a board made mostly of cards.
///
/// # `pixel` is one device pixel in the units this list is drawing in
///
/// A card is pushed into the **board view**, where a unit is a *world* unit — so the arrow's
/// minimum weight cannot be the literal `1.0` it used to be. That floored it at one world
/// unit, which below zoom 1 is less than a pixel, and a fitted board is typically well under
/// zoom 1. On exactly the view a whole board is normally looked at, the round plate survived
/// and the arrow inside it went sub-pixel and disappeared: the *"a badge with no glyph looks
/// like a rendering fault"* state the comment on `weight` claims to prevent, produced by the
/// line that claims it. Passed in rather than derived here, because this function does not
/// know which view it is being pushed into.
fn push_open_badge(
    list: &mut DrawList,
    (at, extent): ([f32; 2], [f32; 2]),
    rotation: f32,
    opacity: f32,
    theme: &crate::theme::Theme,
    hovered: bool,
    pixel: f32,
) {
    let side = extent[0].min(extent[1]);
    if side <= 0.0 {
        return;
    }
    // Hovering **inverts** the badge: the accent fills the plate and the arrow is drawn in
    // the colour the plate used to be. A tint or a shadow would be the softer choice and
    // is the wrong one here — the badge sits on photographs, so any treatment that depends
    // on the background showing through is invisible on half the cards this board is made
    // of. An inversion is legible on anything.
    //
    // A step rather than a fade: an eased hover needs a per-item clock and repaints while
    // the pointer sits still, which is the cost `docs/05-design-language.md` weighs against
    // every animation in this app. The state changes the instant the pointer crosses the
    // badge, which is what "I am on it" has to mean anyway.
    // **Dark, not the accent.** It used to be `theme.accent`, which on a build where the user
    // has chosen the blue put a heavy blue arrow in the top corner of every card — the loudest
    // thing on a board made mostly of cards. Miro's is a thin near-black outline that reads as
    // a control rather than as decoration. The muted ink is the same one the site name uses,
    // so the two pieces of card chrome agree.
    let (plate, ink) =
        if hovered { (theme.accent, theme.surface) } else { (theme.surface, theme.text_muted) };
    // The plate. **A rounded square**, matching Miro's. It was a circle, on the reasoning that
    // a disc never lines up with the picture's own edges — true, and it made the badge read as
    // a bubble stuck on the card rather than as a button belonging to it. The radius is small
    // enough that the shape is unambiguous and large enough not to echo the card's own corner.
    list.push_quad(
        QuadInstance::solid(at, extent, plate)
            .with_corner_radius(side * BADGE_RADIUS)
            .with_border(if hovered { theme.accent } else { theme.border }, 1.0)
            .with_rotation(rotation)
            .with_opacity(opacity),
    );

    // Half the mark's extent, so the unit coordinates below run -1..1 about the centre.
    #[expect(clippy::cast_possible_truncation, reason = "a glyph is screen-scale")]
    let half = (side as f64 * BADGE_GLYPH * 0.5) as f32;
    // At least one device pixel: below that the mark stops being drawn at all rather than
    // being drawn faintly, and a badge with no glyph looks like a rendering fault.
    let weight = (side * BADGE_WEIGHT).max(pixel);
    let centre = [at[0] + extent[0] * 0.5, at[1] + extent[1] * 0.5];

    // One stroked segment between two points of the unit box, as a rotated quad. The list has
    // no path primitive — everything here is a quad — which is also why the mark is geometry
    // rather than a character: U+2197 and U+29C9 are outside every face bundled with this
    // build, so a card asked to shape one draws tofu (`CLAUDE.md` trap 10).
    let mut seg = |a: (f32, f32), b: (f32, f32)| {
        let (ax, ay) = (centre[0] + a.0 * half, centre[1] + a.1 * half);
        let (bx, by) = (centre[0] + b.0 * half, centre[1] + b.1 * half);
        let (dx, dy) = (bx - ax, by - ay);
        // Plus one weight so the round caps land *on* the ends rather than short of them,
        // which is what closes the corners of the box below without mitring anything.
        let length = dx.hypot(dy) + weight;
        list.push_quad(
            QuadInstance::solid(
                [(ax + bx) * 0.5 - length * 0.5, (ay + by) * 0.5 - weight * 0.5],
                [length, weight],
                ink,
            )
            .with_corner_radius(weight * 0.5)
            .with_rotation(rotation + dy.atan2(dx))
            .with_opacity(opacity),
        );
    };

    // Miro's mark, which is the standard "open in a new place" one: a box with its top-right
    // corner missing and an arrow leaving through the gap. Coordinates are Lucide's
    // `square-arrow-out-up-right` mapped from its 24-unit box onto -1..1, y down.
    //
    // The box, anticlockwise from the break in its right-hand side.
    seg((0.75, 0.08), (0.75, 0.75));
    seg((0.75, 0.75), (-0.75, 0.75));
    seg((-0.75, 0.75), (-0.75, -0.75));
    seg((-0.75, -0.75), (-0.08, -0.75));
    // The arrow through the gap, and its two barbs. They run straight left and straight down
    // from the tip — an arrowhead on a 45° shaft is axis-aligned, which is why this needs no
    // trigonometry beyond what `seg` already does.
    seg((-0.17, 0.17), (0.75, -0.75));
    seg((0.25, -0.75), (0.75, -0.75));
    seg((0.75, -0.75), (0.75, -0.25));
}

// `cover_uv` is `vellum_project::look::cover_uv` — shared with the browser painter, which
// crops a card's picture into the same band by the same rule.

// `CARD_LINE_HEIGHT` and `ellipsise` are `vellum_project::card`'s — see the import. Note what
// the deleted copy of `ellipsise`'s doc comment claimed: *"a budget under two leaves no room
// for both a character and the ellipsis, so the whole string is kept"*. Its own body has
// answered `""` at 0 and `"…"` at 1 since the overflow that reasoning caused was fixed, so the
// sentence described the **bug** rather than the function. The `locked: false` trap exactly,
// and one more reason a measurement should exist once.
// `CARD_PADDING` is `vellum_project::look::CARD_PADDING`, measured off Miro's own card and
// shared with the browser painter. It is the single number that decides a card's proportions,
// which is exactly why it must not exist twice.

/// Where a link card's pieces sit, in the item's own space with its top-left at `(0, 0)`.
///
/// One function so the four things that have to agree — the preview image, the favicon, the
/// muted provider line and the title/blurb block — are laid out once. They are drawn by three
/// different paths (an image instance, a second image instance, and two text blocks through
/// `block`), and before this each worked out its own position from `CARD_PADDING`, which is how
/// a card ends up with its text over its picture.
///
/// The order is **Miro's**, from the reference screenshots: the preview image at the top, then
/// a row of favicon-plus-site-name, then the bold title, then the blurb. Every box is inset by
/// the card's padding, and the image has padding *under* it too so the site name does not sit
/// against it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct CardLayout {
    /// The preview image: `(x, y, width, height)`. `None` in the two modes that draw none, and
    /// for a `Large` card whose image has not been fetched.
    image: Option<(f64, f64, f64, f64)>,
    /// The site icon, a square at the start of the provider row.
    favicon: Option<(f64, f64, f64, f64)>,
    /// The provider row's text box, already moved right of the favicon.
    provider: (f64, f64, f64, f64),
    /// The title, at [`TITLE_SCALE`], in the card's own ink.
    title: (f64, f64, f64, f64),
    /// The blurb, at [`BLURB_SCALE`], muted, under the title.
    blurb: (f64, f64, f64, f64),
    /// Title and blurb together.
    ///
    /// Kept alongside the two boxes it is the union of, because the *greeking* path stands in
    /// for the whole text area with one stack of bars and has no business knowing the card is
    /// split — at a zoom where nothing is shaped, a title bar and a blurb bar are the same
    /// mark. Only the two shaped blocks read `title` and `blurb`.
    body: (f64, f64, f64, f64),
    /// The **open-page badge**, a square in the card's top-right corner. `None` when the card
    /// carries no address, when the item is too small to hold one, or — deliberately — never
    /// for anything that is not a link card.
    ///
    /// *"on the top right corner of each widget have an button that will take me to the
    /// website itsel or the link itself"*. Until this, Open was in the properties panel and
    /// on the right-button menu and nowhere on the card, which is the discoverability gap
    /// `CLAUDE.md`'s known-defects list had recorded as *"there is no ↗ open badge on the
    /// card itself"*.
    ///
    /// It is `pub(crate)` along with the rest of this type because the **press path reads
    /// it** — `crate::actions::badge_under` asks this same function where the badge is rather
    /// than reproducing the arithmetic, which is the rule `draw::kanban_runs` already
    /// established: a second copy of a layout is a click that lands somewhere the paint is
    /// not.
    pub(crate) badge: Option<(f64, f64, f64, f64)>,
    /// The **▶ play button**, centred on the poster of a card whose page is a video.
    ///
    /// *"the YouTube previews on Miro I like more because I can just open it and view the
    /// video right then and there."* Velm cannot: playing it needs a browser engine, which
    /// `docs/01-architecture.md` §1 rules out and which the user chose against once the cost
    /// was named — a view that would not zoom with the board, would not be occluded by a
    /// frame, would not export and could not be photographed by `--screenshot`. So this opens
    /// the page in their own browser, exactly as the ↗ badge does.
    ///
    /// It is still worth drawing, and not as decoration: it is the difference between a card
    /// you can *see* is a video and one you have to read to find out. `None` for every card
    /// whose page is not a video, and for a card with no poster to centre it on — a ▶ over a
    /// text block is a button pointing at nothing.
    pub(crate) play: Option<(f64, f64, f64, f64)>,
}

/// The favicon's edge length, as a multiple of the card's font size. Miro's is about level
/// with the cap height of the site name beside it.
const FAVICON_SCALE: f64 = 1.15;

/// The open-page badge's edge length, as a multiple of the card's font size.
///
/// Larger than the favicon because this one is a **target**, not a label: it has to be
/// comfortable to hit with a mouse at a working zoom, where the favicon only has to be
/// recognisable. At the reference card's 13-unit type this is ~23 units square.
const BADGE_SCALE: f64 = 1.75;

/// How much of the badge the mark occupies, as a fraction of its edge.
///
/// The rest is the plate's margin. Raised from 0.34 with the mark: a bare arrow reads at a
/// smaller size than an outlined box does, because the box's own strokes are what you see
/// first. Still short of the rim — a glyph that reaches it reads as a box with a line in it
/// rather than as a button with something on it.
const BADGE_GLYPH: f64 = 0.46;

/// The badge plate's corner radius, as a fraction of its edge.
///
/// Miro's is a rounded square. This was `0.5` — a circle — and the reasoning for it was sound
/// (a disc never lines up with the picture's own edges) and produced something that read as a
/// bubble stuck onto the card rather than a control belonging to it.
const BADGE_RADIUS: f32 = 0.22;

/// The mark's stroke weight, as a fraction of the badge's edge.
///
/// Thin, because it is a line drawing rather than a filled arrow — the same intent as
/// `vellum_ui::widgets::ICON_STROKE`'s 1.5 px, expressed as a fraction because this one is
/// drawn on the board and has to hold up at any zoom. Floored at one device pixel by the
/// caller, which is not the same as one *world* unit — see `push_open_badge`.
const BADGE_WEIGHT: f32 = 0.075;

// A card's type scale and the air between its three voices — `TITLE_SCALE`, `PROVIDER_SCALE`,
// `TITLE_LINES`, `BLURB_LINES`, `PROVIDER_GAP`, `TITLE_GAP`, `BLURB_SCALE` and
// `BLURB_BOTTOM_AIR` — are `vellum_project::card`'s, and are imported at the top of this file.
// The two constants left below are the ones that genuinely belong to *this* painter: a video
// card's caption and its ▶ are drawn here and have no counterpart in the browser.

/// …and how many a **video** card gets, where the picture is the point.
///
/// **One.** It was two, at the user's first request — *"there should only be 2 small lines of
/// text"* — and they then sent a card with the reservation visible as a band of empty white
/// under a one-line title, asking for the thumbnail to be *very* big. A title that needs a
/// second line is ellipsised instead, which is what a grid of thumbnails does everywhere:
/// every line reserved here is a line taken off the frame that says what the video is.
const VIDEO_TITLE_LINES: f64 = 1.0;

/// How much of the poster's short edge the ▶ takes.
///
/// A quarter: big enough to be an obvious target and to read as a play button at a fitted
/// zoom, small enough that the frame behind it — which is what tells you *which* video it is —
/// stays legible around it. Miro's sits at roughly the same fraction.
const PLAY_FRACTION: f64 = 0.25;

// `chars_per_line` and `estimated_lines` are `vellum_project::card`'s, and so is the
// `WRAP_EFFICIENCY` they are discounted by — which is private there, because the two functions
// are the only things that ever needed it and a shared constant nobody reads is a third way to
// disagree. It is the difference between how many characters *fit* on a line and how many a
// word wrap puts there, and it is why one estimate can serve both the clip and the reservation.

#[expect(
    clippy::fn_params_excessive_bools,
    clippy::too_many_arguments,
    reason = "one layout, and every caller must pass the same facts or the paint and the \
              press disagree about where a card's pieces are"
)]
pub(crate) fn card_layout(
    width: f64,
    height: f64,
    font_size: f64,
    mode: CardMode,
    has_image: bool,
    has_favicon: bool,
    has_link: bool,
    is_video: bool,
    title_chars: usize,
) -> CardLayout {
    let pad = width * CARD_PADDING;
    let line = font_size * CARD_LINE_HEIGHT;

    // The badge, in every mode. Bounded by the card as well as by the type scale: on a card
    // dragged down to nothing a badge sized purely from the font would be larger than the
    // item it belongs to and would stick out of it.
    let badge_side = (font_size * BADGE_SCALE).min(width * 0.22).min(height * 0.5);
    // The inset is derived from the **width**, so on a wide, short card `pad` alone could
    // push the badge out through the bottom even though `badge_side` itself was clamped by
    // the height. Clamped to whatever room is actually left, and floored at zero so a
    // degenerate card produces no badge rather than an inverted one.
    let badge_top = pad.min((height - badge_side).max(0.0));
    let badge_fits = has_link && badge_side >= 1.0 && height > badge_side;

    // A collapsed row is one line and nothing else: no image, no second block. The favicon
    // still leads it, which is what makes a row of them scannable.
    if matches!(mode, CardMode::Link) {
        let icon = font_size * FAVICON_SCALE;
        let (favicon, text_left) = if has_favicon {
            (Some((pad, pad + (line - icon) / 2.0, icon, icon)), pad + icon + pad * 0.5)
        } else {
            (None, pad)
        };
        // Centred on the row rather than pinned to the top: a collapsed card *is* one row,
        // so its "top-right corner" and the row's right-hand end are the same place.
        let badge = badge_fits.then(|| {
            let y = (pad + (line - badge_side) / 2.0).min((height - badge_side).max(0.0));
            (width - pad - badge_side, y, badge_side, badge_side)
        });
        // …and the text stops before it. Without this the site name runs underneath the
        // badge, which on a row whose whole job is one legible line is the worst place for
        // it to happen.
        let text_right = badge.map_or(pad, |_| pad + badge_side + pad * 0.5);
        // Nothing below the row. A zero-height box is what `block` reads as "draw nothing",
        // and all three of these are that: a collapsed card *is* its provider row — title
        // included — so the title, the blurb and their union are each empty here rather than
        // each drawing a second copy of the one line the mode exists to be.
        let nothing = (pad, pad + line, (width - pad * 2.0).max(1.0), 0.0);
        return CardLayout {
            image: None,
            favicon,
            provider: (text_left, pad, (width - text_left - text_right).max(1.0), line),
            title: nothing,
            blurb: nothing,
            body: nothing,
            badge,
            // A collapsed row draws no picture, so there is nothing to centre a ▶ on.
            play: None,
        };
    }

    // The image, inset on all four sides rather than bled to the card's edge — Miro's is a
    // picture *in* a card, with the card's surface visible around it and its own rounded
    // corners. A full-bleed one reads as a photo with a caption stuck underneath.
    // **A video card is a poster with a caption**, and its picture is sized from what the
    // caption needs rather than from a fraction of the card.
    //
    // *"for youtube thumbnails there should only be 2 small lines of text, the rest should be
    // the thumbnail, and the 2 lines should be bolded."* A fraction cannot promise that: at
    // `Large`'s 62% the text block is whatever 38% comes to, which is three title lines and a
    // blurb on a tall card and one clipped line on a short one. Measuring the *text* and
    // giving the picture the remainder is the only arrangement where "exactly two lines, and
    // the rest is the thumbnail" is true at every card size.
    //
    // The blurb goes with it — a video's `og:description` is the uploader's sponsor read
    // (*"Get the sponsor's app here: https://exmpl.co/07-Abcd…"* on a real card), which is the least
    // useful text on the board and was taking room from the frame that says what the video is.
    let title_line_for = |scale: f64| font_size * scale * CARD_LINE_HEIGHT;
    let poster = is_video && has_image && mode.shows_image();
    let image = (mode.shows_image() && has_image).then(|| {
        let h = if poster {
            // **Everything the caption does not need.** *"make the image thumbnail very big
            // on only the youtube cards"*, and *"put the writing all the way down"* — so the
            // caption is measured, pinned to the bottom, and the poster takes the rest.
            //
            // The caption is a provider row and *one* title line rather than two: a video
            // title that needs a second line gets an ellipsis instead, which is what a
            // thumbnail grid does everywhere. Two lines was reserved whether or not the title
            // used them, and on a real card that reservation was visible as a band of
            // empty white under the words.
            // Three pads, not two and a half: above the picture, between it and the caption,
            // and below the last line. Shorting it by half a pad made the title box come out
            // at *zero* — the caption did not fit, so `whole_lines` correctly offered no line
            // at all and the card drew a poster and a provider row with no title under it.
            // The gap under the provider row is part of the caption too. Leaving it out was
            // worth exactly one failing test: `y` moved down by `pad * PROVIDER_GAP` and the
            // reservation did not, so `body_h` came up short of a single line and
            // `whole_lines` correctly answered **zero** — a poster with a site name and no
            // title under it. Precisely the failure the comment above already describes,
            // reached by adding air rather than by shorting a pad.
            let caption =
                line + pad * PROVIDER_GAP + title_line_for(TITLE_SCALE) * VIDEO_TITLE_LINES + pad;
            (height - caption - pad * 2.0).max(1.0)
        } else {
            // Per mode: a `Large` card is mostly picture, a `Card` is about half. See
            // `CardMode::image_fraction`.
            (height * mode.image_fraction() - pad).max(1.0)
        };
        (pad, pad, (width - pad * 2.0).max(1.0), h)
    });
    let mut y = image.map_or(pad, |(_, iy, _, ih)| iy + ih + pad);

    let icon = font_size * FAVICON_SCALE;
    let (favicon, text_left) = if has_favicon {
        (Some((pad, y + (line - icon) / 2.0, icon, icon)), pad + icon + pad * 0.5)
    } else {
        (None, pad)
    };

    // Top-right, which over a picture is exactly where Miro puts it. It sits *on* the image
    // rather than beside it — the badge draws its own plate, so it stays legible over
    // whatever the page's `og:image` happens to be.
    let badge = badge_fits.then_some((width - pad - badge_side, badge_top, badge_side, badge_side));

    // The site name has to stop before the badge, but **only when they share a row**. With a
    // picture above, the provider sits well below the badge and clipping it there would throw
    // away characters for a collision that cannot happen — which is precisely the kind of
    // unconditional safety margin that reads as a layout bug once you notice the titles are
    // short for no reason.
    let collides = |top: f64| badge.is_some_and(|(bx, by, _, bh)| top < by + bh && pad < bx);
    let text_right = if collides(y) { pad + badge_side + pad * 0.5 } else { pad };
    let provider = (text_left, y, (width - text_left - text_right).max(1.0), line);
    y += line + pad * PROVIDER_GAP;

    // **The body has to yield too**, and this is the half the first version missed. The
    // badge is `BADGE_SCALE` (1.75) line-heights tall against a provider row of one, so on
    // a card with no picture it reaches *past* the site name into the first line of the
    // title — which is the line the user is most likely to be reading. Asked of the body's
    // own top rather than assumed from the provider's answer, because the two rows are at
    // different heights and only one of them may be under the badge.
    let body_right = if collides(y) { pad + badge_side + pad * 0.5 } else { pad };

    // **The body splits into a title and a blurb**, because Miro's card is a large bold title
    // over a smaller grey description and one block cannot be both: `DrawList::push_layout`
    // takes a single colour *and a single size* for the whole run, and `SpanStyle` has no size
    // field at all — which is the same constraint that already put the provider row in its own
    // block. Two tones was the reason for the second block; two *sizes* is the reason for the
    // third.
    //
    // The title takes as many lines as it needs up to `TITLE_LINES`, and the blurb gets what
    // is left. Title-first rather than a fixed share, because on this board the title is the
    // part that carries the meaning — the reference board's Alibaba cards have sixty-word
    // titles and a blurb that repeats them — and a card too short for both should lose the
    // blurb rather than truncate the name of the thing.
    let body_w = (width - pad - body_right).max(1.0);
    let body_h = (height - y - pad).max(0.0);
    let title_line = font_size * TITLE_SCALE * CARD_LINE_HEIGHT;

    // **Both boxes hold a whole number of lines, and that is what keeps the words inside the
    // card.** *"the writing falls out of the card here"* — the blurb was given whatever height
    // was left over, and a box 2.4 lines tall draws a third line that is 40% inside the card
    // and the rest of the way out through the bottom of it. The character-capacity clip
    // upstream cannot prevent this: it estimates from the *advance*, so it decides roughly how
    // much text to shape and never where the last line lands.
    //
    // Snapping down is the whole fix. A box that is an exact multiple of its own line height
    // either fits a line or does not offer the room for one.
    // `whole_lines` is `vellum_project::card::whole_lines`, shared with the browser painter.
    // It was a closure here, body-identical to that function, and the epsilon's whole
    // explanation lived in this copy — it has moved with it rather than been left behind.

    // **The lines the title needs, not the lines it may have.** `TITLE_LINES` was taken flat,
    // so the box was three lines tall whatever was in it: a one-line title left two empty lines
    // of card between itself and the blurb, and a three-line title left none. The user
    // photographed the first case — a band of white under *"Crush 80 Reboot Pro"* — while
    // asking for *more* white space, which is the tell that this gap was never read as spacing.
    // It was read as a broken card, because air a reader cannot account for always is.
    //
    // Estimated rather than shaped, for the reason `chars_per_line` gives. A poster card still
    // takes its fixed one line: there the reservation is the *point*, since the picture below
    // is sized from whatever the caption does not use.
    let lines = if poster {
        VIDEO_TITLE_LINES
    } else {
        estimated_lines(title_chars, body_w, title_line / CARD_LINE_HEIGHT).clamp(1.0, TITLE_LINES)
    };
    let title_h = whole_lines(body_h.min(title_line * lines), title_line);
    let blurb_line = font_size * BLURB_SCALE * CARD_LINE_HEIGHT;
    // The blurb hangs off the title rather than touching it — see `TITLE_GAP`. Taken out of the
    // blurb's room rather than added to the card, so the item's own box is untouched: *"without
    // changing the over all size"*.
    let title_gap = if poster { 0.0 } else { pad * TITLE_GAP };
    // …and the blurb keeps a line's worth of air beneath it. The bottom `pad` alone is derived
    // from the card's *width*, so on a tall narrow card it is a hairline — and text that ends
    // flush against an edge reads as clipped even when every glyph is inside the box.
    // A poster card has no blurb at all — a zero-height box is what `block` reads as "draw
    // nothing", so `card_text`'s blurb arm is never asked for one.
    //
    // …and it is capped at `BLURB_LINES` rather than taking everything left over. *"less
    // decription"*: a tall card used to give it six or more lines, which is a paragraph with a
    // heading rather than a card. The lines it gives up are what pay for the two gaps above.
    let blurb_room = if poster {
        0.0
    } else {
        (body_h - title_h - title_gap - blurb_line * BLURB_BOTTOM_AIR)
            .max(0.0)
            .min(blurb_line * BLURB_LINES)
    };
    // Centred on the **poster**, not on the card: Miro puts it over the picture, and a ▶
    // floating in a block of text is a button aimed at nothing. Sized from the band rather
    // than from the type, because it is a target on an image and has to stay proportionate to
    // it — a 250-wide card's poster gives about 34 units, comfortably hittable at a working
    // zoom and still small enough to leave the picture readable behind it.
    let play = image.filter(|_| is_video).map(|(ix, iy, iw, ih)| {
        // Clamped to the card, the way `badge_top` is. The image band is derived from the
        // card's height and its own padding, and on a wide, short card that arithmetic can
        // put the band — and so the button centred on it — past the card's own edge, where it
        // is painted outside the item and can never be pressed.
        let side = (iw.min(ih) * PLAY_FRACTION).clamp(1.0, height.max(1.0));
        let x = (ix + (iw - side) / 2.0).clamp(0.0, (width - side).max(0.0));
        let y = (iy + (ih - side) / 2.0).clamp(0.0, (height - side).max(0.0));
        (x, y, side, side)
    });
    CardLayout {
        image,
        favicon,
        provider,
        title: (pad, y, body_w, title_h),
        blurb: (pad, y + title_h + title_gap, body_w, whole_lines(blurb_room, blurb_line)),
        body: (pad, y, body_w, body_h),
        badge,
        play,
    }
}

/// Whether a card has a preview image in the blob store.
///
/// Asked separately from the mode because the two answer different questions: the mode is what
/// the user chose, and this is what there is to draw. A `Large` card with no image falls back
/// to the `Card` layout rather than reserving a band for a picture that is not there.
pub(crate) fn has_thumbnail(kind: &ItemKind) -> bool {
    match kind {
        ItemKind::LinkPreview { thumbnail, .. } | ItemKind::Embed { thumbnail, .. } => {
            thumbnail.is_some()
        }
        _ => false,
    }
}

/// Whether a card has somewhere to open, which is what decides whether it wears a badge.
///
/// A card with no `url` is reachable: an `Embed` imported from a widget Miro had already
/// failed to resolve carries a title and nothing else. A badge on one would be a button that
/// answers *"that card's address is not a web page"* when pressed, which is worse than no
/// button at all.
pub(crate) fn has_link(kind: &ItemKind) -> bool {
    // `host_of`, not `is_some`. The badge is a button, and `ActiveState::open_in_browser`
    // refuses anything that is not http(s) — so gating on the field alone drew a badge on
    // `mailto:` and on Miro's own `about:`-shaped placeholders whose only response to a
    // press is a toast saying no. That is precisely the dead button this function's own
    // doc comment says it exists to prevent, arrived at by trusting the wrong predicate.
    link_url(kind).is_some_and(|url| vellum_link::host_of(url).is_some())
}

// `strip_site_affix` is `vellum_project::card`'s. It is one of the two functions in this file
// that has aborted the whole application on real board data — twice, on a byte index that
// landed inside a character — so the tests pinning both aborts now sit beside the single copy
// of the code rather than beside one of two.

/// A card's address, for the badge and for the press that lands on it.
pub(crate) fn link_url(kind: &ItemKind) -> Option<&str> {
    match kind {
        ItemKind::LinkPreview { url, .. } | ItemKind::Embed { url, .. } => url.as_deref(),
        _ => None,
    }
}

/// Whether a card has a site icon to draw. Decides how far the provider row is indented.
pub(crate) fn has_favicon(kind: &ItemKind) -> bool {
    match kind {
        ItemKind::LinkPreview { favicon, .. } | ItemKind::Embed { favicon, .. } => {
            favicon.is_some()
        }
        _ => false,
    }
}

/// Which display mode a card is in — [`CardMode::Card`] for anything that is not one.
///
/// A `Document` reaches the card painter too and has no mode of its own; it draws as an
/// ordinary card, which is what a PDF placeholder should look like.
pub(crate) fn card_mode(kind: &ItemKind) -> CardMode {
    match kind {
        ItemKind::LinkPreview { mode, .. } | ItemKind::Embed { mode, .. } => *mode,
        _ => CardMode::Card,
    }
}

/// The text a card shows: its title, then its link, as two styled runs.
///
/// Built here rather than stored on the item because it is a *presentation* of
/// several document fields — `vellum_doc::ItemKind::LinkPreview` deliberately keeps
/// them apart, and an absent description is a different thing from an empty one.
/// A card's own text fields, decoded.
///
/// **One source, because two readers now need it.** `card_text` builds the spans and
/// `drawn_title_chars` counts the title's length so `card_layout` can reserve the right number
/// of lines for it — and a layout that reserves for a different string than the painter draws
/// is exactly the empty band the reservation was changed to remove. Same rule as
/// `draw::kanban_runs`: the press path calls the layout rather than reproducing it.
fn card_fields(
    kind: &ItemKind,
) -> (Option<String>, Option<String>, Option<String>, Option<String>, CardMode) {
    let (title, url, description, provider, mode) = match kind {
        ItemKind::LinkPreview { title, url, description, provider, mode, .. } => {
            (title.clone(), url.clone(), description.clone(), provider.clone(), *mode)
        }
        ItemKind::Embed { title, url, description, provider, mode, .. } => {
            (title.clone(), url.clone(), description.clone(), provider.clone(), *mode)
        }
        ItemKind::Document { page_count, .. } => (
            Some(format!("PDF · {page_count} page{}", if *page_count == 1 { "" } else { "s" })),
            None,
            None,
            None,
            CardMode::Card,
        ),
        _ => (None, None, None, None, CardMode::Card),
    };

    // **Decoded here as well as at the two doors, because the doors do not reach what is
    // already inside.** The importer decodes what a paste brings in and `vellum-link` decodes
    // what a fetch brings back — and neither touches the text already written into the cards
    // on the ~58 boards on disk. The user photographed a YouTube card reading
    // `I built the device that Apple wouldn&#39;t…` on a build carrying both of those fixes.
    //
    // Doing it at paint time repairs every existing board with **no migration and no write**,
    // which matters more than the tidiness of a single decode point: RULE ZERO's whole
    // posture is that a board is production data belonging to someone else, and a display-time
    // fix cannot corrupt one. It is idempotent — a decoded string holds no entity for a second
    // pass to find — and it is cheap: three short strings on the cards that are on screen.
    let decode = |value: Option<String>| value.map(|v| vellum_link::decode_entities(&v));
    (decode(title), decode(url), decode(description), decode(provider), mode)
}

/// How many characters the title block will actually **draw**, which is what `card_layout`
/// reserves lines for.
///
/// Not `title.chars().count()`. The painter decodes entities and then strips the site affix
/// before it shapes, so `&#39;` is five stored characters and one drawn one, and
/// *"Amazon.com : Superbat 3G/6G SDI Cable"* loses its first eleven. Counting the stored string
/// over-reserves — which is the band under a one-line title, arriving by a second route.
///
/// A card with no title falls back to its URL, as `card_text` does; one with neither draws
/// nothing and needs no line at all.
pub(crate) fn drawn_title_chars(kind: &ItemKind) -> usize {
    let (title, url, _, provider, _) = card_fields(kind);
    match (&title, &url) {
        (Some(title), _) => strip_site_affix(title, provider.as_deref()).chars().count(),
        (None, Some(url)) => url.chars().count(),
        (None, None) => 0,
    }
}

fn card_text(kind: &ItemKind, slot: u16, line_budget: usize) -> vellum_text::StyledText {
    let (title, url, description, provider, mode) = card_fields(kind);

    let mut spans = Vec::new();
    match slot {
        // The provider row: the site's name, on its own, so it can be drawn in its own muted
        // colour. A **collapsed** card puts its title here too, because the row *is* the card's
        // one line and there is no second block beneath it to hold one.
        BlockKey::SECONDARY => {
            let name = provider.unwrap_or_default();
            // The row is one line by construction, so its budget is the per-line one — the
            // caller's `line_budget` is the *block's* capacity, which for this block is a line.
            if matches!(mode, CardMode::Link) {
                let lead = title.or_else(|| url.clone()).unwrap_or_default();
                let prefix = if name.is_empty() { String::new() } else { format!("{name}  ") };
                // Clipped, not wrapped. One line is the entire point of the collapsed row, and
                // a title long enough to wrap would otherwise make it two — at which point it
                // is a small `Card` with no blurb rather than a row.
                let room = line_budget.saturating_sub(prefix.chars().count());
                if !prefix.is_empty() {
                    spans.push(DocSpan::plain(prefix));
                }
                spans.push(DocSpan::new(ellipsise(&lead, room), vellum_doc::SpanStyle::bold()));
            } else if !name.is_empty() {
                spans.push(DocSpan::plain(ellipsise(&name, line_budget)));
            }
        }

        // The title, then the page's own blurb. The **site name is not here** — it is the row
        // above, in its own colour. The URL is not repeated either: the title and the site
        // already say what this is, and a wrapped URL is three lines of tracking parameters. It
        // is still in the document, still in the panel, and still where a click goes.
        BlockKey::PRIMARY => match mode {
            // Drawn entirely by the row above. Nothing here, rather than a duplicate of it.
            CardMode::Link => {}
            CardMode::Card | CardMode::Large => {
                // **Every span is clipped against the block, not just the blurb.**
                //
                // The blurb was the only one that was, and the other two overflow just as
                // readily — Miro's imported cards routinely carry a *title* that is the
                // raw address, and the `else` branch below appended a whole URL with no
                // clip at all. An Amazon link is 400 characters of tracking parameters
                // with no spaces in it, so it cannot wrap at a word boundary and pours
                // straight out through the bottom of the card and down the board.
                // Reported twice: once as *"the images are all distoreted"*'s neighbour,
                // and again as *"i still have the links overflowing problem"* after a fix
                // that only addressed the font size.
                //
                // `room` is what the block has left after what is already in it, so the
                // three spans share one budget rather than each assuming the whole of it.
                fn room(spans: &[DocSpan], budget: usize) -> usize {
                    let used: usize = spans.iter().map(|s| s.text.chars().count()).sum();
                    budget.saturating_sub(used)
                }
                // The title alone. Its blurb moved to `CARD_BLURB_SLOT` when the card gained a
                // third block, so that the two can be set at different sizes — `room` is kept
                // because the fallback below still shares this block's budget.
                let _ = room(&spans, line_budget);
                // **Bold, and it never was.** Two doc comments in this file have described "a
                // large bold title" since the third block was added, and both arms below wrote
                // `DocSpan::plain` — so the card's hierarchy rested on colour alone, and with
                // the site name set at the same size as the title the whole card read as one
                // paragraph. That is what the user was looking at when they said Velm "looks a
                // little bit off" with Miro open beside it.
                //
                // Real weight rather than a different face: Inter Bold is bundled and
                // `set_sans_serif_family` points the alias at it, so `family_has_bold` no longer
                // drops the request and silently leaves the family (trap 10).
                match (&title, &url) {
                    (Some(title), _) => {
                        let trimmed = strip_site_affix(title, provider.as_deref());
                        let clipped = ellipsise(trimmed, line_budget);
                        spans.push(DocSpan::new(clipped, vellum_doc::SpanStyle::bold()));
                    }
                    // No title: the URL is the only name it has.
                    (None, Some(url)) => {
                        let clipped = ellipsise(url, line_budget);
                        spans.push(DocSpan::new(clipped, vellum_doc::SpanStyle::bold()));
                    }
                    (None, None) => {}
                }
            }
        },

        // The blurb, under the title and set smaller — see `CARD_BLURB_SLOT`.
        CARD_BLURB_SLOT => {
            // **A video card has none**, and this is a content decision rather than a layout
            // one, which is why it is here and not left to a zero-height box: `FitBox::new`
            // clamps to 1.0, so an empty box still shapes a line and draws it clipped. A
            // video's `og:description` is the uploader's sponsor read — *"Get Opera here:
            // https://exmpl.co/07-Abcd…"* on a real card — which is the least useful
            // text on the board, and the room it was taking now belongs to the poster.
            let is_video = url.as_deref().is_some_and(vellum_link::plays_video);
            if !is_video && matches!(mode, CardMode::Card | CardMode::Large) {
                match description.filter(|d| !d.trim().is_empty()) {
                    // Skipped when it only repeats the title, which is 18 of the 91 cards on
                    // the reference board — see `says_the_same_as`.
                    Some(description) if !says_the_same_as(title.as_deref(), &description) => {
                        spans.push(DocSpan::plain(ellipsise(&description, line_budget)));
                    }
                    // Nothing to say about the page, so the address is worth the room. Only
                    // when there *is* a title, or this repeats what the title block just drew.
                    _ if title.is_some() => {
                        if let Some(url) = url {
                            spans.push(DocSpan::plain(ellipsise(&url, line_budget)));
                        }
                    }
                    _ => {}
                }
            }
        }

        // A card has two slots and no more.
        _ => {}
    }
    text::convert(&DocText::from_spans(spans))
}

// `says_the_same_as` is `vellum_project::card`'s — the other of the two string rules that
// decide what a card actually says, and the other one whose shape has cost this application a
// process. Measured from the reference capture (`captures/reference-board.html`): 18 of the 91
// `preview` widgets carry an `openGraph.description` that *is* the `openGraph.title`, 15 of
// them alibaba.com. Miro draws one of the two; we drew both, and the user photographed a card
// saying the same sixty-word sentence twice.

// ── The Agent Canvas ─────────────────────────────────────────────────────────
//
// Four kinds — an agent, a note, a file tree, a browser — and one way of drawing them.
// `docs/07-agent-canvas.md` is the contract; what follows is the painting half of features
// 1, 2, 3 and 14.
//
// # Every rectangle comes from the node's own `layout()`
//
// `crate::agent::layout`, `crate::note::layout`, `crate::filetree::layout` and
// `crate::browser::layout` are each called by the press path as well as by this file —
// `ActiveState::node_part_under` is the one place that asks. Nothing here computes a position
// of its own for anything that can be pressed: the `draw::kanban_runs` / `CardLayout::badge`
// rule, which this repository has paid for twice, a second copy of a layout being a click that
// lands where the paint is not.
//
// ⚠ **This paragraph was written before it was true.** For three waves only the *agent* layout
// had a caller: `NoteLayout::hit`, `BrowserLayout::hit` and `filetree::row_at` were each
// written, tested and reached from nothing, so a note's footer, a browser's Reload, its
// address, its *Open externally* button — which `crate::browser` calls "the whole answer when
// no engine is running" — and **every disclosure triangle on every file tree** were drawn and
// could not be pressed. A tree could not be expanded at all. A comment asserting a call that
// does not happen is the `locked: false` trap in prose; if a fifth kind is added here, check
// it against the press path rather than against this paragraph.
//
// A file tree needs one thing more than a layout, which is why [`NodePaint::tree_rows`]
// exists: the rows that were *drawn* are not the rows the tree holds — `visible_rows()` bounds
// them and one is held back for the "n more" line — so the press path asks `tree_paint` for
// the rectangles it actually painted rather than asking `row_at` for a row that is not on
// screen.
//
// The one place that rule is *widened* rather than obeyed is [`NodePaint`], which flattens a
// transcript into positioned runs, pictures and option cards. It is `pub(crate)` and pure for
// exactly the reason `kanban_runs` is: the press path has to be able to ask the same function
// which option card a point is over.
//
// # A layout is asked for the node's **scaled** size
//
// `layout()`'s own comment says its arguments are the item's size *before* the placement's
// scale and leaves applying the scale to the painter. Applying it by asking for the scaled
// size — rather than laying out unscaled and multiplying every rectangle afterwards — is what
// makes `too_small` mean what it says: a node scaled to a fifth really is too small to hold a
// header, and a layout that decided otherwise would hand back controls nobody can hit. **The
// press path must ask the same way**, or the two disagree by exactly the scale factor.

/// The body size an Agent Canvas node draws at, in world units, at
/// [`NODE_REFERENCE_WIDTH`]. `docs/05-design-language.md` §5's body size, which is also what a
/// link card is set in.
const NODE_FONT_SIZE: f64 = 13.0;

/// The width [`NODE_FONT_SIZE`] is calibrated against — `crate::agent::DEFAULT_SIZE.0`, which
/// that constant's own comment sizes for about sixty characters to the line at 13 units.
const NODE_REFERENCE_WIDTH: f64 = 520.0;

/// Line spacing inside a node, as a multiple of the font size.
///
/// The card's, not prose's: a transcript is a stack of short paragraphs and labels. The two
/// have to be the same number because an option card *is* a card drawn inside a node.
const NODE_LINE_HEIGHT: f64 = CARD_LINE_HEIGHT;

/// The mean advance of a proportional face, as a fraction of the font size.
///
/// Used to estimate how many characters fit on a line **without shaping any of them**, which
/// is the only way to decide how tall a transcript entry is before deciding whether it is
/// worth shaping at all. `card_layout` already relies on the same figure and records that it
/// is good to within a few percent on a proportional face.
///
/// It is *not* good for CJK or emoji, which are about twice this wide — so the estimate
/// under-counts lines and a run could overflow its box. That is why every run is also
/// **clipped by character count** to what its own box was measured for: the clip is what
/// stands between a wrong estimate and text pouring out of the node, exactly as it does on a
/// card.
const NODE_MEAN_ADVANCE: f64 = 0.5;

/// The most lines any single run will hold before it is ellipsised.
///
/// A bound on the *measurement*, not a design choice: `wrap_estimate` takes the head of the
/// string before it asks how long it is, so this is what stops a fifty-megabyte answer costing
/// its own length on the paint path.
const MAX_TEXT_LINES: usize = 64;

/// A node's text size at its own width, so the painter and the press path agree on one number.
///
/// The same shape as [`card_font_size`], and for the same reason: a node dragged out to four
/// times its default size should carry text legible from wherever it was dragged to. Clamped
/// at 1.0 below, so a node made *narrower* keeps 13-unit type and simply fits fewer words
/// rather than shrinking its words until nothing is legible at any zoom.
pub(crate) fn node_font_size(width: f64) -> f64 {
    NODE_FONT_SIZE * (width / NODE_REFERENCE_WIDTH).clamp(1.0, 6.0)
}

/// The role label's size, as a multiple of the node's body size.
const ROLE_SCALE: f64 = 1.0;
/// The line under the role — what the agent is doing, or what it runs on.
const SUBTITLE_SCALE: f64 = 0.82;
/// Scaffolding: a tool call, a thought, a turn boundary, a file's own name.
const SCAFFOLD_SCALE: f64 = 0.88;

/// The most lines one transcript entry is given before it is ellipsised.
///
/// A bound rather than a budget. An agent can emit a single ten-thousand-word answer, and a
/// node whose newest entry filled the whole box would push everything else off the top —
/// including the question it is answering. Eight lines is a paragraph, which is as much as
/// anyone reads on a canvas before opening the transcript properly.
const MAX_ENTRY_LINES: usize = 8;

/// The most transcript entries laid out for one node.
///
/// The real bound is the box — entries are measured from the newest backwards and stop when it
/// is full — and this is the backstop for a node dragged out to ten thousand units tall, where
/// "what fits" is hundreds. `crate::agent_view` already bounds the *tail*; this bounds the
/// **drawing**, which is a different limit and the one that decides the frame's cost.
const MAX_ENTRIES: usize = 40;

/// The most option cards drawn side by side.
///
/// Feature 14 is *"here are three UI directions I built"*. Four is one more than that; past it
/// each card is narrower than its own title and the row stops being scannable. The rest are
/// counted in a line underneath rather than silently dropped.
const MAX_OPTION_CARDS: usize = vellum_agent::transcript::MAX_CHOICES;

/// How much of an option card is picture, when any of the choices carries one.
const OPTION_IMAGE_FRACTION: f64 = 0.56;

/// A transcript picture's height as a fraction of the transcript's width.
///
/// 9:16, which is the shape a screenshot of a window most often is. The texture is drawn
/// through [`cover_uv`] into whatever this reserves, so the band's aspect decides the crop and
/// never the picture's — the fix feedback 23 records for link cards, arrived at here before it
/// could be got wrong a second time.
const NODE_IMAGE_ASPECT: f64 = 0.5625;

/// How a run of text on a node is coloured.
///
/// A *role*, not a colour. The palette is applied where the run is drawn, so the layout stays
/// pure and can be built — and tested — with no `Theme` in the room. That is the division
/// `KanbanRun::muted` already makes, widened because a transcript has more than two voices.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tone {
    /// The agent's answer, a note's body, a file's name. What you are here to read.
    Primary,
    /// Scaffolding: a tool call, a thought, a turn boundary, a footer.
    Muted,
    /// Another agent speaking. Not this agent's own words, and a board where the two look
    /// alike is one you cannot follow.
    Accent,
    /// A failure. The one thing a user in Clean mode most needs to see, which is why
    /// `TranscriptEvent::visible_in_clean_mode` refuses to hide it.
    Failed,
    /// A question only a person can answer.
    NeedsYou,
}

/// One run of text a node draws, and where.
///
/// The same shape as [`KanbanRun`] and for the same reason: how many labels a node draws
/// depends on its data, so a slot's meaning is an index into a flattened list rather than
/// arithmetic over events.
#[derive(Debug, Clone)]
pub(crate) struct NodeRun {
    /// In the item's own space, top-left at `(0, 0)`, already scaled.
    rect: NodeRect,
    text: String,
    font_size: f64,
    tone: Tone,
    align: Align,
    /// What the text cache is keyed on for this run.
    ///
    /// **A transcript's words are not in the document**, so `Projected::generation` — which
    /// only moves when the *item* changes — cannot speak for them: a node whose agent said
    /// something new would keep drawing the previous sentence, at the same slot, forever. The
    /// cache compares stamps for equality and nothing else, so a stamp derived from the run's
    /// own content is a legitimate answer to a question the document cannot answer.
    stamp: u64,
}

/// A tinted rectangle drawn behind a node's text.
#[derive(Debug, Clone, Copy)]
struct NodePlate {
    rect: NodeRect,
    tone: PlateTone,
    radius: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlateTone {
    /// A recess — the prompt row, a file tree's list, an address bar.
    Well,
    /// A surface floating on the node: an option card, a browser's page stand-in.
    Card,
    /// The option card the user picked.
    Chosen,
    /// An error's wash.
    Failed,
    /// A question's wash.
    NeedsYou,
    /// The narrow bar beside a message that came from another agent.
    Rail,
}

/// A picture inside a node, by content hash into the shared blob store.
///
/// Addressed exactly as a pasted screenshot is — `docs/07` §4 — so deduplication and the
/// 268MB residency budget come for free and nothing new is cached anywhere.
#[derive(Debug, Clone)]
struct NodeImage {
    rect: NodeRect,
    blob: String,
}

/// One selectable option an agent offered — feature 14.
///
/// `pub(crate)` with public geometry because **the press path resolves a click against this
/// same list**, through [`NodePaint::option_at`]. Reproducing the arithmetic there instead is
/// the `kanban_runs` failure in a new place: a card that lights up under the pointer while the
/// click selects its neighbour.
///
/// The press path reads `rect`, `choice` and `chosen`. **`event` is recorded and not read**,
/// and the lint is allowed for that one field: it names which `TranscriptEvent::Options` a
/// card belongs to, which is what a future *"which question was this"* needs and is free to
/// record while the list is being built. Writing the geometry only when the click arrives is
/// the alternative, and it is the mistake that made `opens_context_menu` a defect.
#[derive(Debug, Clone)]
pub(crate) struct OptionCard {
    /// The card's box in the item's own space, already scaled.
    pub(crate) rect: NodeRect,
    /// **Which question this card answers**, by the prompt that asked it.
    ///
    /// ⚠ This was the card's index into `AgentView::events` and was `#[allow(dead_code)]` —
    /// dead because that index cannot cross the seam it needed to: the view is the
    /// mode-filtered, `VIEW_EVENTS`-bounded list, and `choose_option` searches the node's
    /// whole ring, which is numbered differently. So a click carried only its choice *id*,
    /// and with two unanswered option sets on one node — an agent that asked twice, which is
    /// exactly what a long run does — a shared id like `yes` answered whichever the search
    /// reached first rather than the card that was pressed.
    ///
    /// The prompt is an identity that survives the seam because it is *in* the event on both
    /// sides. Not a generated id, because these events are also read back off disk, where
    /// nothing would have assigned one.
    pub(crate) prompt: String,
    /// `Choice::id`, sent back verbatim when the user picks this one.
    pub(crate) choice: String,
    /// Already chosen, so the row records an answer rather than offering one again.
    pub(crate) chosen: bool,
}

/// The two answers to a permission request: where they are drawn, and where they are pressed.
///
/// **One pair of rectangles, read twice.** The painter fills them in the same statement that
/// records them, and `ActiveState::press_in_transcript` resolves a click against this list
/// rather than repeating the arithmetic — the `kanban_runs` rule, which here has been broken
/// in both directions and the second one was worse:
///
/// - A permission *drawn* and not *answered* leaves the agent blocked for ever behind a
///   button, which is the inert control this codebase refuses to ship. That is what the
///   comment this replaces was about, and it is the failure that did **not** happen.
/// - A permission *answered* and not *drawn* is what shipped: the row was reserved and the
///   chips pushed with no plate and no words, so a blank strip across the body of the card
///   allowed on the left half and denied on the right — and the press arm returns `true`, so
///   the node could not even be selected by clicking there.
///
/// "Measured now, painted later" is not half a feature; it is a hot zone with nothing on it.
#[derive(Debug, Clone)]
pub(crate) struct PermissionChips {
    /// `RequestId`'s string, which is what an answer is sent back with.
    pub(crate) request: String,
    pub(crate) allow: NodeRect,
    pub(crate) deny: NodeRect,
}

/// Everything an Agent Canvas node draws that is not its own card.
///
/// Built once per frame per **visible** node — an off-screen one is never asked, because the
/// R-tree never hands it to the paint loop, which is `docs/07` §5d's rule that a culled agent
/// keeps running and stops being drawn.
#[derive(Debug, Clone, Default)]
pub(crate) struct NodePaint {
    runs: Vec<NodeRun>,
    plates: Vec<NodePlate>,
    images: Vec<NodeImage>,
    options: Vec<OptionCard>,
    permissions: Vec<PermissionChips>,
    /// A file tree's disclosure triangles: the box, and whether the directory is open.
    twisties: Vec<(NodeRect, bool)>,
    /// A file tree's rows, so the press path can resolve a click against the rows that were
    /// actually **drawn**.
    ///
    /// The `kanban_runs` rule, and it is sharper here than anywhere else in this file: the
    /// painter draws `visible_rows()` of them, holds one back for the *"n more"* line, and
    /// numbers them from a scroll offset. A press path that re-derived any of those three
    /// would expand the wrong directory — or, as it was, expand none at all.
    tree_rows: Vec<TreeRowHit>,
    /// Which run is the prompt row, when that row has the keyboard.
    ///
    /// `None` for every node that is not being typed into, which is all of them but one. The
    /// index is into [`NodePaint::runs`] and is turned into a text *slot* by
    /// [`NodePaint::prompt_slot`], so the painter's caret lands on the run whose string the
    /// caret's offsets actually index.
    prompt_run: Option<usize>,
}

/// One drawn file-tree row, as the press path needs it.
#[derive(Debug, Clone)]
pub(crate) struct TreeRowHit {
    /// The whole row, in the item's own space.
    pub(crate) rect: NodeRect,
    /// The disclosure triangle, for a directory. `None` for a file.
    pub(crate) twisty: Option<NodeRect>,
    /// The entry's path relative to the tree's root — the form `FileTreeModel::expanded`
    /// stores, so expanding is a call rather than a conversion.
    pub(crate) relative: String,
    /// The absolute path, for revealing or opening the file.
    pub(crate) path: std::path::PathBuf,
    pub(crate) is_dir: bool,
}

impl NodePaint {
    /// Which option card a point in the item's own space is over, if any.
    ///
    /// The press path's question, answered from the rectangles the painter drew.
    pub(crate) fn option_at(&self, x: f64, y: f64) -> Option<&OptionCard> {
        self.options.iter().find(|card| card.rect.contains(x, y))
    }

    /// Every card, for the tests that check they tile without overlapping.
    #[cfg(test)]
    pub(crate) fn options(&self) -> &[OptionCard] {
        &self.options
    }

    pub(crate) fn permissions(&self) -> &[PermissionChips] {
        &self.permissions
    }

    /// How many text slots this node claims.
    pub(crate) fn runs(&self) -> usize {
        self.runs.len()
    }

    /// The text slot the prompt row's caret belongs in, if this node's row has the keyboard.
    ///
    /// A *slot*, not a run index, because that is what `Painter::block` and
    /// [`crate::draw::TextCursor`] speak — and deriving it here rather than at the two call
    /// sites is what keeps the `CELL_SLOT_BASE` offset in one place.
    pub(crate) fn prompt_slot(&self) -> Option<u16> {
        let index = u16::try_from(self.prompt_run?).ok()?;
        index.checked_add(CELL_SLOT_BASE)
    }

    /// Every tree row that was **drawn**, in order.
    ///
    /// `pub(crate)` for the same reason `kanban_runs` is: a caller that wants to aim at a row
    /// — the press path, or a `--demo` fixture driving a real press at one — must ask the
    /// list the painter produced rather than rebuild it from `row_rect` and a bound.
    pub(crate) fn tree_rows(&self) -> &[TreeRowHit] {
        &self.tree_rows
    }

    /// Which drawn tree row a point in the item's own space is over, and whether it landed on
    /// the disclosure triangle.
    ///
    /// The press path's question, answered from the rectangles the painter drew — see
    /// [`TreeRowHit`].
    pub(crate) fn tree_row_at(&self, x: f64, y: f64) -> Option<(&TreeRowHit, bool)> {
        let row = self.tree_rows.iter().find(|row| row.rect.contains(x, y))?;
        let on_twisty = row.twisty.is_some_and(|box_| box_.contains(x, y));
        Some((row, on_twisty))
    }

    fn run(&self, index: usize) -> Option<&NodeRun> {
        self.runs.get(index)
    }

    fn shift(&mut self, dy: f64) {
        for run in &mut self.runs {
            run.rect.y += dy;
        }
        for plate in &mut self.plates {
            plate.rect.y += dy;
        }
        for image in &mut self.images {
            image.rect.y += dy;
        }
        for card in &mut self.options {
            card.rect.y += dy;
        }
        for chips in &mut self.permissions {
            chips.allow.y += dy;
            chips.deny.y += dy;
        }
        for (rect, _) in &mut self.twisties {
            rect.y += dy;
        }
        for row in &mut self.tree_rows {
            row.rect.y += dy;
            if let Some(twisty) = row.twisty.as_mut() {
                twisty.y += dy;
            }
        }
    }

    fn absorb(&mut self, other: Self) {
        // **`runs` first, and the offset taken before it.** An absorbed paint's run indices
        // move by however many runs are already here, so anything that *names* an index —
        // `prompt_run`, and `OptionCard::event`'s neighbour `prompt_slot` — has to be
        // rebased. Today only `agent_paint` sets `prompt_run`, and it sets it on the paint
        // doing the absorbing rather than on one being absorbed; this keeps that true if a
        // later caller does the opposite.
        let offset = self.runs.len();
        if self.prompt_run.is_none() {
            self.prompt_run = other.prompt_run.map(|index| index + offset);
        }
        self.runs.extend(other.runs);
        self.plates.extend(other.plates);
        self.images.extend(other.images);
        self.options.extend(other.options);
        self.permissions.extend(other.permissions);
        self.twisties.extend(other.twisties);
        self.tree_rows.extend(other.tree_rows);
    }

    /// Adds a run, clipped to what its own box was measured for.
    ///
    /// The clip is not tidiness: it is the only thing standing between an under-estimated
    /// line count — which [`NODE_MEAN_ADVANCE`] guarantees for CJK and emoji — and text drawn
    /// out through the bottom of the node and across the board, which is a fault this file has
    /// already recorded twice on link cards.
    fn text(&mut self, rect: NodeRect, text: &str, font_size: f64, tone: Tone, align: Align) {
        if rect.is_empty() || font_size <= 0.0 || text.trim().is_empty() {
            return;
        }
        let line = font_size * NODE_LINE_HEIGHT;
        // **The nudge is not defensive.** These heights are built by multiplying a line height
        // by a line count, so a box sized for exactly three lines arrives as 52.649999999999
        // against a line of 17.55 and `floor` answers two. `card_layout`'s `whole_lines` has
        // the same epsilon for the same reason, measured on a real card.
        let fits = ((rect.height / line) + 1e-6).floor().max(1.0) as usize;
        let (clipped, _) = wrap_estimate(text, rect.width, font_size, fits);
        if clipped.trim().is_empty() {
            return;
        }
        let stamp = fnv1a(clipped.as_bytes(), rect.width.to_bits() ^ font_size.to_bits());
        self.runs.push(NodeRun { rect, text: clipped, font_size, tone, align, stamp });
    }

    /// Adds a run holding a **field's exact string**, unclipped, and answers where it went.
    ///
    /// [`NodePaint::text`] ellipsises to what the box was measured for, which is right for a
    /// transcript — an agent's answer is as long as it likes — and wrong for a field with a
    /// caret in it: the caret's offsets index the buffer, so a shaped string that has been cut
    /// short puts the caret at the wrong character, or past the end of the string entirely.
    /// `crate::draw`'s kanban path already keeps the raw value beside the drawn one for
    /// exactly this, and this is the same split by a shorter route.
    ///
    /// It also pushes an **empty** run, which `text` refuses to do. That is feedback 25 — the
    /// caret that would not appear in an empty sticky — arriving in a fourth place: no run
    /// means no block, no block means no origin, and `push_caret` has nothing to measure from.
    fn field(&mut self, rect: NodeRect, text: &str, font_size: f64, tone: Tone, align: Align) -> usize {
        let stamp = fnv1a(text.as_bytes(), rect.width.to_bits() ^ font_size.to_bits());
        self.runs.push(NodeRun {
            rect,
            text: text.to_owned(),
            font_size,
            tone,
            align,
            stamp,
        });
        self.runs.len() - 1
    }

    fn plate(&mut self, rect: NodeRect, tone: PlateTone, radius: f32) {
        if !rect.is_empty() {
            self.plates.push(NodePlate { rect, tone, radius });
        }
    }
}

/// A 64-bit FNV-1a, seeded.
///
/// **Not BLAKE3**, for the reason `docs/07` §4 gives about the transcript directory's own key:
/// this names a cache entry, nothing verifies it, and a collision costs one frame of stale
/// text rather than anything anyone could exploit. It is also the only hash affordable on the
/// paint path — it runs over every run of every visible node.
fn fnv1a(bytes: &[u8], seed: u64) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64 ^ seed;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// How many lines `text` needs in a box `width` wide at `size`, and the text clipped to what
/// those lines can hold.
///
/// Estimated from [`NODE_MEAN_ADVANCE`] rather than measured, and the order is the point: this
/// decides *what to shape*, so measuring it would be the shaping pass it exists to avoid.
///
/// **Everything is bounded by characters before it is bounded by anything else.** Transcript
/// text is the least controlled string in the application — it is whatever a third-party
/// process wrote — and `[profile.release]` sets `panic = "abort"`, so `strip_site_affix`'s two
/// aborts (feedback 30) are the standing precedent. `chars()` throughout, and the head is
/// taken *before* the length is asked for, so a fifty-megabyte answer costs the budget rather
/// than its own length.
fn wrap_estimate(text: &str, width: f64, size: f64, max_lines: usize) -> (String, usize) {
    if width <= 0.0 || size <= 0.0 {
        return (String::new(), 0);
    }
    let per_line = ((width / (size * NODE_MEAN_ADVANCE)).floor() as usize).max(1);
    let cap = max_lines.clamp(1, MAX_TEXT_LINES);
    let budget = per_line.saturating_mul(cap);
    // One character past the budget, so "did this have to be cut" is answerable without
    // walking the rest of the string.
    let head: String = text.chars().take(budget.saturating_add(1)).collect();
    // Newlines count: an agent's answer arrives with them, and an estimate that ignored them
    // would report a four-paragraph reply as one line of the same character count.
    let lines: usize = head
        .lines()
        .map(|line| line.chars().count().div_ceil(per_line).max(1))
        .sum::<usize>()
        .max(1);
    let lines = lines.min(cap);
    (ellipsise(&head, per_line.saturating_mul(lines)), lines)
}

/// One transcript entry, built at `y = 0` so it can be placed once its height is known.
struct Entry {
    paint: NodePaint,
    height: f64,
}

/// A column of runs stacked down a box, in the item's own space.
struct Column {
    paint: NodePaint,
    x: f64,
    width: f64,
    y: f64,
    font: f64,
}

impl Column {
    fn new(x: f64, width: f64, font: f64) -> Self {
        Self { paint: NodePaint::default(), x, width, y: 0.0, font }
    }

    /// Adds a paragraph and returns how tall it came out.
    fn paragraph(&mut self, text: &str, scale: f64, tone: Tone, max_lines: usize) -> f64 {
        let size = self.font * scale;
        let line = size * NODE_LINE_HEIGHT;
        let (_, lines) = wrap_estimate(text, self.width, size, max_lines);
        if lines == 0 {
            return 0.0;
        }
        let height = line * lines as f64;
        self.paint.text(
            NodeRect::new(self.x, self.y, self.width, height),
            text,
            size,
            tone,
            Align::Left,
        );
        self.y += height;
        height
    }

    fn gap(&mut self, by: f64) {
        self.y += by;
    }

    fn finish(self) -> Entry {
        Entry { paint: self.paint, height: self.y }
    }
}

/// Where one transcript entry's pieces go, given its width and the room left for it.
///
/// One `match` rather than a function per event kind, so *"a tool call is a muted line and a
/// message is a railed block"* is a rule stated once rather than six functions that happen to
/// agree about padding.
fn transcript_entry(
    // The event's position in the view, which nothing reads now that an option card carries
    // its question instead of an index into a list `choose_option` does not share. Kept as a
    // parameter rather than removed from the signature: it is the natural place for any
    // future per-entry identity, and the caller already has it.
    _index: usize,
    event: &TranscriptEvent,
    x: f64,
    width: f64,
    font: f64,
    room: f64,
) -> Entry {
    let rail = font * 0.25;
    let indent = font * 0.75;
    let pad = font * 0.4;
    // How many lines of body a box `height` tall can hold. Taken from the room actually left
    // rather than from the node's whole transcript box, so an entry can never be measured
    // against space it has not got — which is what puts the last line of one outside the node.
    let lines_in = |height: f64| {
        let line = font * NODE_LINE_HEIGHT;
        if line <= 0.0 { 1 } else { ((height / line).floor() as usize).max(1) }
    };
    let prose_lines = lines_in(room).min(MAX_ENTRY_LINES);
    // A plated entry loses its own padding before it counts lines, for the same reason.
    let plated_lines = lines_in(room - pad * 2.0).min(MAX_ENTRY_LINES);
    let mut column = Column::new(x, width, font);

    match event {
        // Prose the agent addressed to the user. This *is* the answer, so it is the one thing
        // in the transcript set in the primary ink at the full body size.
        TranscriptEvent::Text { text } => {
            column.paragraph(text, 1.0, Tone::Primary, prose_lines);
        }

        // Scaffolding, one muted line each. `headline` is the away-mode digest's own wording,
        // reused rather than restated: two spellings of *"ran bash"* is two places for the
        // vocabulary to drift, and that function is already bounded and already panic-safe on
        // multi-byte text — which matters more here than anywhere, since this is where a tool's
        // raw output arrives.
        TranscriptEvent::Thought { .. }
        | TranscriptEvent::ToolCall { .. }
        | TranscriptEvent::ToolResult { .. }
        | TranscriptEvent::Terminal { .. }
        | TranscriptEvent::TurnStarted { .. }
        | TranscriptEvent::TurnEnded { .. }
        | TranscriptEvent::PermissionAnswer { .. } => {
            column.paragraph(&event.headline(), SCAFFOLD_SCALE, Tone::Muted, 1);
        }

        // Another agent talking, or this one talking to another. Attributed in the accent and
        // railed, because `docs/07` §6's whole point is that a wired-up board is followable:
        // a message drawn like the agent's own words would make two nodes read as one
        // conversation with no author.
        TranscriptEvent::Message { from, text }
        | TranscriptEvent::MessageSent { to: from, text } => {
            let sent = matches!(event, TranscriptEvent::MessageSent { .. });
            // **Words, not an arrow.** `→` is U+2192 and this file has been burned twice by
            // assuming a character is in whatever face `font_family: None` resolves to (trap
            // 10, and the `↗` and `▶` that had to become geometry). *from* and *to* are two
            // characters longer and cannot draw as tofu on anybody's machine.
            let who = if sent { format!("to {}", from.name) } else { format!("from {}", from.name) };
            let mut body = Column::new(x + rail + indent, (width - rail - indent).max(1.0), font);
            body.paragraph(&who, SUBTITLE_SCALE, Tone::Accent, 1);
            body.paragraph(text, 1.0, Tone::Primary, prose_lines);
            let mut entry = body.finish();
            entry.paint.plate(NodeRect::new(x, 0.0, rail, entry.height), PlateTone::Rail, 0.0);
            // Shifted onto the column before it is absorbed. A no-op today, because this is
            // the only content in its entry — and the line that keeps it true if anything is
            // ever put above it, which is how a plate ends up half a paragraph out of place.
            entry.paint.shift(column.y);
            column.paint.absorb(entry.paint);
            column.y += entry.height;
        }

        // A failure, on its own wash. Prominent deliberately: Clean mode is exactly where a
        // silent failure reads as an agent that simply never replied.
        TranscriptEvent::Error { message } => {
            let mut body = Column::new(x + pad, (width - pad * 2.0).max(1.0), font);
            body.gap(pad);
            body.paragraph(message, 1.0, Tone::Failed, plated_lines);
            body.gap(pad);
            let mut entry = body.finish();
            entry
                .paint
                .plate(NodeRect::new(x, 0.0, width, entry.height), PlateTone::Failed, CARD_RADIUS);
            entry.paint.shift(column.y);
            column.paint.absorb(entry.paint);
            column.y += entry.height;
        }

        // A question the agent is blocked on. It has to read as something *waiting* rather than
        // as something that happened — the agent is stopped until a person acts, and nothing
        // else in a transcript is.
        TranscriptEvent::PermissionRequest { id, summary, detail } => {
            let chip = font * NODE_LINE_HEIGHT * 1.4;
            let mut body = Column::new(x + pad, (width - pad * 2.0).max(1.0), font);
            body.gap(pad);
            body.paragraph("Waiting for your answer", SUBTITLE_SCALE, Tone::NeedsYou, 1);
            body.paragraph(summary, 1.0, Tone::Primary, plated_lines.min(2));
            if !detail.trim().is_empty() {
                body.paragraph(detail, SCAFFOLD_SCALE, Tone::Muted, 1);
            }
            // **The answers, drawn from the two rectangles the press path reads.** Not two
            // sets of arithmetic that agree today: [`PermissionChips`] carries these very
            // rects, so a button cannot come to be drawn where a press does not land — the
            // `kanban_runs` rule, and the one this row broke in the other direction. It was
            // shipped measured-but-unpainted, which made the reserved band a blank strip that
            // *answered* a permission: the left half allowed and the right half denied, and
            // the arm returns `true`, so the node could not even be selected there.
            let chip_w = ((body.width - pad) / 2.0).max(1.0);
            let allow = NodeRect::new(body.x, body.y + pad, chip_w, chip);
            let deny = NodeRect::new(body.x + chip_w + pad, body.y + pad, chip_w, chip);
            body.gap(pad + chip + pad);
            let mut entry = body.finish();
            entry.paint.permissions.push(PermissionChips {
                request: id.0.clone(),
                allow,
                deny,
            });
            entry.paint.plate(
                NodeRect::new(x, 0.0, width, entry.height),
                PlateTone::NeedsYou,
                CARD_RADIUS,
            );
            // **After the wash, because plates are drawn in the order they are pushed.**
            // `push_node_plate` walks `NodePaint::plates` in order, so a chip painted with the
            // body would come out *under* the question's own tint. Runs are a separate pass and
            // are always above both, which is why only the plates have to be ordered.
            //
            // Allow wears the accent border an *option card the user picked* wears and deny the
            // plain card: nothing new is invented for two buttons, and the same plate vocabulary
            // the rest of the node uses is what makes them read as pressable.
            entry.paint.plate(allow, PlateTone::Chosen, CARD_RADIUS);
            entry.paint.plate(deny, PlateTone::Card, CARD_RADIUS);
            // The label's own box is inset so its single line sits in the middle of the chip
            // rather than against its top edge — `NodePaint::text` lays a run out from the top
            // of the box it is given, and a plate 1.4 lines tall with the word at the top reads
            // as a mistake rather than as a button.
            let label = font * NODE_LINE_HEIGHT;
            let lift = ((chip - label) / 2.0).max(0.0);
            entry.paint.text(
                NodeRect::new(allow.x, allow.y + lift, allow.width, label),
                "Allow",
                font,
                Tone::Accent,
                Align::Center,
            );
            entry.paint.text(
                NodeRect::new(deny.x, deny.y + lift, deny.width, label),
                "Deny",
                font,
                Tone::Primary,
                Align::Center,
            );
            entry.paint.shift(column.y);
            column.paint.absorb(entry.paint);
            column.y += entry.height;
        }

        // A picture the agent produced. Its band has a fixed aspect and the texture does not,
        // so it goes through `cover_uv` — feedback 23, which is the one thing that stops every
        // screenshot on the board being scaled by a different amount on each axis.
        TranscriptEvent::Image { blob, caption } => {
            // **Clamped to the room left, and its caption's room taken off first.** The band's
            // aspect is a preference and the node's box is not: a 9:16 band on a 500-wide node
            // is 280 units tall, which is most of a default transcript and all of a short one.
            // Clamping costs nothing because the picture is drawn through `cover_uv` — a
            // shorter band crops rather than squashes, which is the whole point of that
            // function.
            let caption_room = if caption.is_some() {
                font * 0.2 + font * SUBTITLE_SCALE * NODE_LINE_HEIGHT
            } else {
                0.0
            };
            let height =
                (width * NODE_IMAGE_ASPECT).min((room - caption_room).max(1.0)).max(1.0);
            column.paint.images.push(NodeImage {
                rect: NodeRect::new(x, 0.0, width, height),
                blob: blob.clone(),
            });
            column.y += height;
            if let Some(caption) = caption {
                column.gap(font * 0.2);
                column.paragraph(caption, SUBTITLE_SCALE, Tone::Muted, 1);
            }
        }

        // Feature 14: *"here are three UI directions I built"*.
        TranscriptEvent::Options { prompt, choices, chosen } => {
            column.paragraph(prompt, 1.0, Tone::Primary, 2);
            column.gap(font * 0.3);
            let shown = choices.len().min(MAX_OPTION_CARDS);
            if shown > 0 {
                let gap = font * 0.5;
                // Floored at a unit rather than at a legible width, deliberately: a floor big
                // enough to be readable is a floor that makes the row wider than the node on a
                // narrow one, which paints outside the item. A row of cards too narrow to read
                // is a row you zoom in on; a row that has left the node is a bug.
                let card_w = ((width - gap * (shown - 1) as f64) / shown as f64).max(1.0);
                let card_pad = font * 0.35;
                // The band is reserved for the *row*, not per card. A row where one card is
                // taller than its neighbours because it happens to carry a picture reads as
                // broken rather than as three answers to one question.
                let has_image = choices.iter().take(shown).any(|choice| choice.image.is_some());
                let band = if has_image { (card_w * OPTION_IMAGE_FRACTION).max(1.0) } else { 0.0 };
                let title_h = font * NODE_LINE_HEIGHT * 2.0;
                let body_h = font * SUBTITLE_SCALE * NODE_LINE_HEIGHT * 2.0;
                let card_h = band + card_pad * 2.0 + title_h + body_h;
                for (slot, choice) in choices.iter().take(shown).enumerate() {
                    let left = x + slot as f64 * (card_w + gap);
                    let rect = NodeRect::new(left, column.y, card_w, card_h);
                    let picked = chosen.as_deref() == Some(choice.id.as_str());
                    column.paint.plate(
                        rect,
                        if picked { PlateTone::Chosen } else { PlateTone::Card },
                        CARD_RADIUS,
                    );
                    if let Some(hash) = &choice.image {
                        column.paint.images.push(NodeImage {
                            rect: NodeRect::new(left, column.y, card_w, band),
                            blob: hash.clone(),
                        });
                    }
                    let inner_x = left + card_pad;
                    // Floored at **zero**, not at one: a card narrower than its own padding
                    // has no room for words, and `NodePaint::text` reads a zero-width box as
                    // "draw nothing". Flooring at one instead would put a one-unit run outside
                    // the card it belongs to, which is `card_layout`'s `FitBox::new(w.max(1.0))`
                    // overflow arriving by the route meant to close it.
                    let inner_w = (card_w - card_pad * 2.0).max(0.0);
                    let mut inner = column.y + band + card_pad;
                    column.paint.text(
                        NodeRect::new(inner_x, inner, inner_w, title_h),
                        &choice.title,
                        font,
                        if picked { Tone::Accent } else { Tone::Primary },
                        Align::Left,
                    );
                    inner += title_h;
                    if let Some(text) = &choice.body {
                        column.paint.text(
                            NodeRect::new(inner_x, inner, inner_w, body_h),
                            text,
                            font * SUBTITLE_SCALE,
                            Tone::Muted,
                            Align::Left,
                        );
                    }
                    column.paint.options.push(OptionCard {
                        rect,
                        prompt: prompt.clone(),
                        choice: choice.id.clone(),
                        chosen: picked,
                    });
                }
                column.y += card_h;
            }
            if choices.len() > shown {
                column.paragraph(
                    &format!("and {} more", choices.len() - shown),
                    SUBTITLE_SCALE,
                    Tone::Muted,
                    1,
                );
            }
        }
    }

    column.finish()
}

/// An agent node's transcript, its header sub-line and its prompt row, flattened.
///
/// # The newest entry is the one that survives
///
/// Entries are measured from the **end backwards** and stop when the box is full, then laid
/// out forwards from the first that fitted. Laying out oldest-first and cutting the overflow
/// is the obvious version and is wrong for a live transcript: the thing that just happened is
/// the thing being watched, and it would be the first casualty. What *is* cut is said so, on
/// screen — the same rule `AgentView::truncated` exists for one layer up.
///
/// What a node says while it is listening, or while its audio is with a transcriber.
///
/// The elapsed seconds are shown rather than a level meter: a meter needs the samples, which
/// are on the capture thread and deliberately never cross to the painter, and *how long have I
/// been talking* is the thing that actually matters against
/// [`vellum_agent::voice::MAX_UTTERANCE_SECONDS`].
///
/// The transcribing line **names the backend**, which is not decoration: it is the one moment
/// the user can tell whether their voice stayed on the machine or went to somebody's API, and
/// leaving them to guess is the wrong default for a feature that opens a microphone.
fn voice_line(status: &crate::voice::VoiceStatus) -> String {
    match status {
        crate::voice::VoiceStatus::Listening { held_ms } => {
            format!("Listening… {}s — let go of ⌥D to put it in the prompt", held_ms / 1000)
        }
        crate::voice::VoiceStatus::Transcribing { backend } => {
            format!("Transcribing with {backend}…")
        }
    }
}

/// Pure, and free rather than a method, for `kanban_runs`' reason: the press path calls it too.
pub(crate) fn agent_paint(view: &AgentView, laid: &AgentLayout, font: f64) -> NodePaint {
    let mut paint = NodePaint::default();
    // A compact node is one target and draws one thing — its role, through the item's own text
    // slot. `AgentLayout::hit` already refuses to offer a control at this size, and a
    // transcript nobody can read is quads nobody can use.
    if laid.too_small {
        return paint;
    }

    // The line under the role. **What it is doing beats what it runs on**: `detail` is live —
    // *"ran 3 tools"*, *"claude is not installed"* — and `subtitle` is configuration that has
    // not changed since the node was made. There is room for one line, and the live one is the
    // one worth having; the provider is in the inspector, derived from the same
    // `agent::subtitle` this falls back to, so the two cannot disagree.
    if !laid.role.is_empty() {
        let size = font * SUBTITLE_SCALE;
        let line = size * NODE_LINE_HEIGHT;
        let top = laid.role.y + laid.role.height - line;
        // **Voice beats both**, for the same reason `detail` beats `subtitle` one step down:
        // there is room for one line and the live one is the one worth having. *Is it hearing
        // me* is the only question a person holding a key down has, and until this the answer
        // was drawn nowhere at all — see `AgentView::voice`.
        let spoken = view.voice.as_ref().map(voice_line);
        let text = match spoken.as_deref() {
            Some(spoken) => spoken,
            None if view.detail.trim().is_empty() => &view.subtitle,
            None => &view.detail,
        };
        // Accent while the microphone is open, so a listening node is legible across the
        // board rather than reading as one more grey subtitle among a dozen.
        let tone = match (&view.voice, view.status) {
            (Some(_), _) => Tone::Accent,
            (None, Status::Error) => Tone::Failed,
            (None, _) => Tone::Muted,
        };
        paint.text(
            NodeRect::new(laid.role.x, top, laid.role.width, line),
            text,
            size,
            tone,
            Align::Left,
        );
    }

    // The prompt row: a well, and either what is being typed, what was half-typed earlier, or
    // an invitation.
    //
    // The draft is held by the runtime rather than by the caret precisely so it survives
    // clicking away from the node — `AgentView::draft` says why — so drawing it is what makes
    // that promise visible rather than merely true.
    if !laid.prompt.is_empty() {
        paint.plate(laid.prompt, PlateTone::Well, CARD_RADIUS);
        let pad = font * 0.4;
        let line = font * NODE_LINE_HEIGHT;
        let inner = NodeRect::new(
            laid.prompt.x + pad,
            laid.prompt.y + (laid.prompt.height - line).max(0.0) / 2.0,
            (laid.prompt.width - pad * 2.0).max(1.0),
            line.min(laid.prompt.height),
        );
        match view.caret {
            // **The keyboard is in this row.** Three things change together and all three are
            // load-bearing: the string is the buffer's own and is *unclipped* (the caret's
            // offsets index it, so an ellipsised copy puts the caret at the wrong character);
            // the run is pushed even when it is empty (feedback 25 — no run, no block, and
            // `push_caret` has nothing to measure from); and the placeholder is suppressed,
            // because "Ask this agent to do something" is not what the buffer holds and
            // drawing it would put the caret inside a sentence the user is not typing.
            Some(_) => {
                paint.prompt_run =
                    Some(paint.field(inner, &view.draft, font, Tone::Primary, Align::Left));
            }
            None => {
                let (text, tone) = if view.draft.trim().is_empty() {
                    ("Ask this agent to do something", Tone::Muted)
                } else {
                    (view.draft.as_str(), Tone::Primary)
                };
                paint.text(inner, text, font, tone, Align::Left);
            }
        }
    }

    let box_ = laid.transcript;
    if box_.is_empty() {
        return paint;
    }

    let gap = font * 0.45;
    // **The notice's line is reserved before anything is measured, not squeezed in after.**
    // Reserving it afterwards is the version that was written first and it puts the last entry
    // exactly one line outside the node — measured against the whole box, then pushed down by
    // a line nobody had budgeted for. Reserving it always costs a line of slack on a
    // transcript that turns out to fit, which is invisible, and the alternative is a two-pass
    // measurement to learn something the first pass has to guess anyway.
    let notice = font * SUBTITLE_SCALE * NODE_LINE_HEIGHT;
    let available = (box_.height - notice).max(0.0);

    // Measured newest-first, so `entries` comes out reversed — which is why it is turned round
    // before anything is placed.
    let mut entries: Vec<Entry> = Vec::new();
    let mut used = 0.0;
    let mut elided = view.truncated || view.events.len() > MAX_ENTRIES;
    for (index, event) in view.events.iter().enumerate().rev().take(MAX_ENTRIES) {
        let entry =
            transcript_entry(index, event.as_ref(), box_.x, box_.width, font, available - used);
        if entry.height <= 0.0 {
            continue;
        }
        // An entry that cannot fit the node at all is skipped rather than drawn over the edge.
        // Prose can never reach this — it is measured in whole lines against the room left —
        // but an option row and a permission plate have a *minimum* height, and a node too
        // short to hold one has to say so rather than paint outside itself.
        //
        // The epsilon is the one `card_layout`'s `whole_lines` already carries and for the
        // same measured reason: an entry clamped to *exactly* the room left is built by
        // subtracting a height and adding it back, so it arrives a fraction of a ULP over and
        // an exact `>` throws away the picture it had just been sized to hold.
        const SLACK: f64 = 1e-6;
        if entry.height > available + SLACK {
            elided = true;
            continue;
        }
        let next = used + entry.height + if entries.is_empty() { 0.0 } else { gap };
        if next > available + SLACK && !entries.is_empty() {
            elided = true;
            break;
        }
        used = next;
        entries.push(entry);
    }
    entries.reverse();

    let mut y = box_.y;
    if elided {
        // Said on the node rather than implied by a scrollbar there is not. A transcript that
        // silently begins in the middle is one the reader believes is the whole story, which
        // is the failure `AgentView::truncated` exists to prevent one layer up.
        //
        // *Some* rather than *earlier*: what is dropped is nearly always the oldest, but an
        // option row too tall for a short node is dropped wherever it sits, and a line that
        // said "earlier" would be wrong about it.
        let size = font * SUBTITLE_SCALE;
        paint.text(
            NodeRect::new(box_.x, y, box_.width, notice),
            "some output is not shown",
            size,
            Tone::Muted,
            Align::Left,
        );
        y += notice;
    }
    for (position, mut entry) in entries.into_iter().enumerate() {
        if position > 0 {
            y += gap;
        }
        entry.paint.shift(y);
        y += entry.height;
        paint.absorb(entry.paint);
    }
    paint
}

/// A note node's body and footer.
///
/// The **title** is not here: it is the item's own `StyledText`, so it goes through the named
/// primary slot and takes the caret, the search index and `Board::set_text` with no new path —
/// which is exactly why `ItemKind::AgentNote` keeps it beside the token rather than inside it.
pub(crate) fn note_paint(
    model: &vellum_agent::NoteModel,
    body: Option<&str>,
    laid: &crate::note::NoteLayout,
    font: f64,
) -> NodePaint {
    let mut paint = NodePaint::default();
    if laid.too_small {
        return paint;
    }

    if !laid.body.is_empty() {
        match body.map(str::trim).filter(|text| !text.is_empty()) {
            Some(text) => paint.text(laid.body, text, font, Tone::Primary, Align::Left),
            // Nothing here is inert: a node with no content says which of the two states it is
            // in, and the two have different remedies.
            None => {
                let line = (font * NODE_LINE_HEIGHT).min(laid.body.height);
                let waiting = if model.path.trim().is_empty() {
                    "This note has no file yet"
                } else {
                    "This note's file has not been read yet"
                };
                paint.text(
                    NodeRect::new(laid.body.x, laid.body.y, laid.body.width, line),
                    waiting,
                    font,
                    Tone::Muted,
                    Align::Left,
                );
            }
        }
    }

    if !laid.footer.is_empty() {
        // The file's name at one end and the scope at the other, so the scope keeps the same
        // column whatever the file is called — the arrangement the board library's own card
        // metadata row had to be rebuilt into (feedback 22) for exactly this reason.
        let size = font * SUBTITLE_SCALE;
        let chip = (laid.footer.width * 0.4).min(size * 6.0);
        paint.text(
            NodeRect::new(
                laid.footer.x,
                laid.footer.y,
                (laid.footer.width - chip).max(1.0),
                laid.footer.height,
            ),
            crate::note::file_name(&model.path),
            size,
            Tone::Muted,
            Align::Left,
        );
        paint.text(
            NodeRect::new(
                laid.footer.x + laid.footer.width - chip,
                laid.footer.y,
                chip,
                laid.footer.height,
            ),
            crate::note::scope_label(&model.scope),
            size,
            Tone::Muted,
            Align::Right,
        );
    }
    paint
}

/// A file tree's header and the rows that fit.
///
/// **Only `TreeLayout::visible_rows()` of them**, which is the rule the whole canvas rests on
/// seen from its sharpest angle: a `target/` directory holds forty thousand entries, and a node
/// that shaped what exists rather than what is on screen would stall the frame it was placed
/// on. `vellum_agent::filetree` has already bounded the *read*; this bounds the **drawing**,
/// and the two limits are different on purpose.
pub(crate) fn tree_paint(
    model: &vellum_agent::FileTreeModel,
    view: Option<&TreeView>,
    laid: &crate::filetree::TreeLayout,
    font: f64,
    // How many rows are scrolled off the top.
    //
    // ⚠ **The list used to draw the first `visible_rows()` and stop**, so on any tree with
    // more entries than fitted, everything below was unreachable: no gesture existed to
    // reach it, and the node's own *"n more"* line named a number the user could do nothing
    // about. Held app-side (`AgentRuntime::tree_scroll`) rather than in the document: a
    // scroll position is not board content, and putting one in the CRDT would add an undo
    // step per wheel notch.
    //
    // The press path passes the **same** offset, and resolves clicks against the rectangles
    // this function records rather than re-deriving them — so a scrolled row is pressed
    // where it is drawn, by construction.
    scroll: usize,
) -> NodePaint {
    let mut paint = NodePaint::default();
    if laid.too_small {
        return paint;
    }

    if !laid.header.is_empty() {
        // `note::file_name` rather than a second last-component split. It exists because a
        // path can carry any character a filesystem allows and this codebase has aborted twice
        // on byte-slicing at a found index; a tree's root is the same string in the same
        // danger, so it gets the same function.
        let root = if model.root.trim().is_empty() {
            "Project root"
        } else {
            crate::note::file_name(&model.root)
        };
        paint.text(laid.header, root, font, Tone::Primary, Align::Left);
    }

    if laid.list.is_empty() {
        return paint;
    }
    paint.plate(laid.list, PlateTone::Well, CARD_RADIUS);

    let Some(view) = view else {
        let line = (font * NODE_LINE_HEIGHT).min(laid.list.height);
        paint.text(
            NodeRect::new(laid.list.x, laid.list.y, laid.list.width, line),
            "This tree has not been read yet",
            font * SUBTITLE_SCALE,
            Tone::Muted,
            Align::Left,
        );
        return paint;
    };

    let fits = laid.visible_rows();
    // Clamped here rather than where it is stored: the row count changes as directories open
    // and close, and a stale offset must degrade to "the last screenful" instead of an empty
    // list. `saturating_sub` gives 0 for a tree that now fits entirely.
    let scroll = scroll.min(view.rows.len().saturating_sub(fits));
    // One row is kept back for the count of what did not fit, so the node's last row is never a
    // file the reader believes is the last file.
    let more = view.rows.len() > scroll + fits || view.truncated;
    let shown = if more { fits.saturating_sub(1) } else { fits };
    for (index, row) in view.rows.iter().skip(scroll).take(shown).enumerate() {
        let rect = crate::filetree::row_rect(laid.list, index, 0);
        let twisty = row
            .entry
            .is_dir
            .then(|| crate::filetree::twisty_rect(rect, row.depth));
        if let Some(twisty) = twisty {
            paint.twisties.push((twisty, row.expanded));
        }
        // Recorded for the press path, from the rectangle that was just drawn. Three things
        // decide which row is where — `visible_rows()`, the row held back for the *"n more"*
        // line, and the scroll offset — and a press path that re-derived any of them would
        // expand the wrong directory. See `NodePaint::tree_rows`.
        paint.tree_rows.push(TreeRowHit {
            rect,
            twisty,
            relative: row.entry.relative.clone(),
            path: row.entry.path.clone(),
            is_dir: row.entry.is_dir,
        });
        let left = crate::filetree::label_x(rect, row.depth);
        paint.text(
            NodeRect::new(left, rect.y, (rect.x + rect.width - left).max(1.0), rect.height),
            &row.entry.name,
            font * SCAFFOLD_SCALE,
            // A gitignored entry is drawn quietly rather than hidden. It is only returned at
            // all when the tree was *asked* to show ignored files, so the user has said they
            // want to see it, and greying it is what tells them why it is unusual.
            if row.entry.ignored { Tone::Muted } else { Tone::Primary },
            Align::Left,
        );
    }
    if more && shown < view.rows.len() {
        paint.text(
            crate::filetree::row_rect(laid.list, shown, 0),
            &format!("{} more", view.rows.len() - shown),
            font * SCAFFOLD_SCALE,
            Tone::Muted,
            Align::Left,
        );
    }
    paint
}

/// A browser node's address and, in place of a page, the reason there is not one.
///
/// `crate::browser::viewport_message` decides the wording — from the two switches
/// `should_run_engine` reads **and** from what the engine pool says about this particular page
/// — so the node, the runtime and the inspector cannot come to disagree about whether a page
/// is live. The failure that join prevents is an engine running behind a card that says there
/// is not one.
pub(crate) fn browser_paint(
    model: &vellum_agent::BrowserModel,
    reason: Option<String>,
    laid: &crate::browser::BrowserLayout,
    font: f64,
) -> NodePaint {
    let mut paint = NodePaint::default();
    if laid.too_small {
        return paint;
    }

    if !laid.address.is_empty() {
        paint.plate(laid.address, PlateTone::Well, CARD_RADIUS);
        let pad = font * 0.35;
        let line = font * NODE_LINE_HEIGHT;
        let inner = NodeRect::new(
            laid.address.x + pad,
            laid.address.y + (laid.address.height - line).max(0.0) / 2.0,
            (laid.address.width - pad * 2.0).max(1.0),
            line.min(laid.address.height),
        );
        let (text, tone) = if model.url.trim().is_empty() {
            ("Enter an address", Tone::Muted)
        } else {
            (model.url.as_str(), Tone::Primary)
        };
        paint.text(inner, text, font * SUBTITLE_SCALE, tone, Align::Left);
    }

    if laid.viewport.is_empty() {
        return paint;
    }
    paint.plate(laid.viewport, PlateTone::Card, CARD_RADIUS);

    // **The page's own surface is not this pass's to draw.** An engine composites outside wgpu
    // — `docs/01-architecture.md` §1 is why there may not be one at all — so what is drawn
    // either way is the page's *name*, and a dormant node reads as the page it points at rather
    // than as an empty rectangle.
    let pad = font * 0.8;
    let mut column =
        Column::new(laid.viewport.x + pad, (laid.viewport.width - pad * 2.0).max(1.0), font);
    column.gap(pad);
    let name =
        if model.title.trim().is_empty() { model.url.as_str() } else { model.title.as_str() };
    column.paragraph(name, 1.0, Tone::Primary, 2);
    if let Some(reason) = reason.as_deref() {
        column.gap(font * 0.3);
        column.paragraph(reason, SUBTITLE_SCALE, Tone::Muted, 2);
    }
    let mut entry = column.finish();
    entry.paint.shift(laid.viewport.y);
    paint.absorb(entry.paint);
    paint
}

/// `vellum_ui::Palette::LIGHT.warning` — `#8A5D0B`.
///
/// Three decimals, the spelling `Theme::with_accent` already uses for the same reason: it is
/// what a `const fn` can take (`Rgba::from_rgb8` is not one) and `Rgba::pack` rounds, so three
/// decimals land on the byte exactly. The test below is what says so rather than this comment.
const STATUS_NEEDS_YOU: Rgba = Rgba::new(0.541, 0.365, 0.043, 1.0);

/// `vellum_ui::Palette::LIGHT.danger` — the `xr-red` `#E65B58` that kept destruction when the
/// teal took selection off it.
const STATUS_FAILED: Rgba = Rgba::new(0.902, 0.357, 0.345, 1.0);

/// The colour a status dot is drawn in.
///
/// **Idle and working come from the palette; the other two do not, and cannot.**
/// `crate::theme::Theme` carries no destructive or cautionary token — the board has never
/// needed one — while `vellum_ui::Palette` has carried both since the accent was split off the
/// coral (feedback 22). They are spelled out above and pinned to that palette by a test, which
/// is the arrangement `Theme::with_accent` already uses for exactly this reason: a hand-copied
/// constant in another crate is `inspect.rs`'s `THEME_BORDER` trap, and the *test* is what
/// makes it safe rather than the comment.
///
/// # Working wears the accent, and the accent is a setting
///
/// `Accent::Red` **is** `Palette::danger`, so a user who chose the coral gets a working dot and
/// a failed dot in one colour. That collision is why the distinction is not carried by hue at
/// all: the two states that want a *person* — needs-you and failed — draw a halo ring around
/// the dot, and the two that do not never do. See [`push_status_dot`].
fn status_colour(status: Status, theme: &Theme) -> Rgba {
    match status {
        Status::Idle => theme.text_muted,
        Status::Running => theme.accent,
        Status::WaitingForPermission => STATUS_NEEDS_YOU,
        Status::Error => STATUS_FAILED,
    }
}

/// How much of its own paper a node lays over a background picture.
///
/// High on purpose. The picture is decoration and the transcript is the point, so this is
/// tuned so that ink on the scrim clears its contrast target over **any** image, including a
/// white one and a black one. Turning it down is how you get a personalised node you cannot
/// read; the lever for "I want to see more of my picture" is the node's own opacity.
const BACKGROUND_SCRIM: f32 = 0.86;

/// The chat theme an agent node draws in, or `None` for anything that is not one.
///
/// The **resolved** theme — the node's own choice, or the app-wide default when it has not
/// made one — through `crate::agent::chat_theme`, which is the single place that fallback is
/// spelled. Only `ItemKind::Agent`: a note, a file tree and a browser are not transcripts,
/// and dressing them in a chat theme would make "ChatGPT" a statement about a directory
/// listing.
fn chat_theme_of(kind: &ItemKind, ctx: &DrawContext<'_>) -> Option<vellum_agent::ChatTheme> {
    let ItemKind::Agent { model, .. } = kind else { return None };
    Some(crate::agent::chat_theme(&crate::agent::decode(model), ctx.default_chat_theme))
}

fn chat_opacity_of(kind: &ItemKind) -> f32 {
    let ItemKind::Agent { model, .. } = kind else { return 1.0 };
    crate::agent::chat_opacity(&crate::agent::decode(model))
}

fn chat_background_of(kind: &ItemKind) -> Option<&str> {
    let ItemKind::Agent { model, .. } = kind else { return None };
    // Borrowed out of the token's own JSON rather than decoded, which would allocate an
    // `AgentModel` per node per frame for a string that is usually absent. The key is
    // `AgentModel::chat_background`'s serde name; a test pins the two together.
    crate::agent::background_hash(model)
}

/// What a [`Tone`] means against a palette.
fn tone_colour(tone: Tone, theme: &Theme) -> Rgba {
    match tone {
        Tone::Primary => theme.text,
        Tone::Muted => theme.text_muted,
        Tone::Accent => theme.accent,
        Tone::Failed => STATUS_FAILED,
        Tone::NeedsYou => STATUS_NEEDS_YOU,
    }
}

/// How present a plate's wash is behind the words on it.
///
/// Low: `docs/05-design-language.md` §3a's rule that legibility beats the material applies to a
/// tint exactly as it does to glass, and an error whose wash made its own message harder to
/// read would be the wrong half kept.
const PLATE_WASH: f32 = 0.10;

/// One of a node's tinted rectangles.
fn push_node_plate(
    list: &mut DrawList,
    at: ([f32; 2], [f32; 2]),
    plate: NodePlate,
    rotation: f32,
    opacity: f32,
    theme: &Theme,
    hairline: f32,
) {
    let (origin, extent) = at;
    if extent[0] <= 0.0 || extent[1] <= 0.0 {
        return;
    }
    let quad = QuadInstance::solid(
        origin,
        extent,
        match plate.tone {
            PlateTone::Well => theme.canvas,
            PlateTone::Card | PlateTone::Chosen => theme.frame_fill,
            PlateTone::Failed => STATUS_FAILED.with_alpha(PLATE_WASH),
            PlateTone::NeedsYou => STATUS_NEEDS_YOU.with_alpha(PLATE_WASH),
            PlateTone::Rail => theme.accent,
        },
    )
    .with_corner_radius(plate.radius)
    .with_rotation(rotation)
    .with_opacity(opacity);
    let quad = match plate.tone {
        PlateTone::Card => quad.with_border(theme.border, hairline),
        // The picked option keeps the accent it was picked with, so a row that has been
        // answered says which answer it got without having to be read.
        PlateTone::Chosen => quad.with_border(theme.accent, hairline.max(1.0) * 2.0),
        _ => quad,
    };
    list.push_quad(quad);
}

/// The status dot, and the halo that says a person is wanted.
///
/// **The halo carries the meaning, not the colour.** `status_colour` explains why: the accent
/// is a user setting and one of its three values collides with the failure colour, so a
/// distinction drawn in hue alone would quietly stop existing for anyone who chose the coral.
/// A ring is legible against every one of them, and it says the right thing — *only the two
/// blocked states want a person* — as a property of the mark rather than of the palette.
fn push_status_dot(
    list: &mut DrawList,
    at: ([f32; 2], [f32; 2]),
    colour: Rgba,
    attention: bool,
    rotation: f32,
    opacity: f32,
) {
    let (origin, extent) = at;
    let side = extent[0].min(extent[1]);
    if side <= 0.0 {
        return;
    }
    if attention {
        let halo = side * 1.9;
        list.push_quad(
            QuadInstance::solid(
                [origin[0] - (halo - extent[0]) / 2.0, origin[1] - (halo - extent[1]) / 2.0],
                [halo, halo],
                Rgba::TRANSPARENT,
            )
            .with_border(colour.with_alpha(colour.a * 0.5), (side * 0.22).max(0.5))
            .with_corner_radius(halo * 0.5)
            .with_rotation(rotation)
            .with_opacity(opacity),
        );
    }
    list.push_quad(
        QuadInstance::solid(origin, [side, side], colour)
            .with_corner_radius(side * 0.5)
            .with_rotation(rotation)
            .with_opacity(opacity),
    );
}

/// The raw/clean toggle: bars on a plate, three for Raw and one for Clean.
///
/// **Bars rather than the words.** The control is 22 world units square and no word fits in it
/// — and a *character* is not an option, because trap 10 is that anything outside a plain sans
/// face draws as tofu and every glyph that would say this (`⚙`, `≡`, `☰`) is exactly that. The
/// bars say what the two modes mean anyway: Raw is everything the agent did, Clean is the
/// answer alone.
fn push_mode_toggle(
    list: &mut DrawList,
    at: ([f32; 2], [f32; 2]),
    raw: bool,
    rotation: f32,
    opacity: f32,
    theme: &Theme,
    hairline: f32,
) {
    let (origin, extent) = at;
    let side = extent[0].min(extent[1]);
    if side <= 0.0 {
        return;
    }
    list.push_quad(
        QuadInstance::solid(origin, extent, if raw { theme.accent } else { theme.surface })
            .with_corner_radius(side * 0.25)
            .with_border(theme.border, hairline)
            .with_rotation(rotation)
            .with_opacity(opacity),
    );

    let ink = if raw { theme.surface } else { theme.text_muted };
    let bars = if raw { 3 } else { 1 };
    let weight = (side * 0.11).max(hairline);
    let width = side * 0.5;
    let pitch = side * 0.22;
    let first = origin[1] + extent[1] / 2.0 - pitch * (bars - 1) as f32 / 2.0 - weight / 2.0;
    for bar in 0..bars {
        list.push_quad(
            QuadInstance::solid(
                [origin[0] + (extent[0] - width) / 2.0, first + bar as f32 * pitch],
                [width, weight],
                ink,
            )
            .with_corner_radius(weight * 0.5)
            .with_rotation(rotation)
            .with_opacity(opacity),
        );
    }
}

/// Run, or stop while a turn is in flight — one button and two meanings, because they are
/// never both available and two buttons would leave one permanently dead.
///
/// A triangle and a square, both geometry. The triangle goes through the mesh batch for
/// `push_play_button`'s reason: it is not expressible as axis-aligned rectangles, `▶` is U+25B6
/// and outside every plain sans face, and the mesh pipeline is the multisampled one, so its
/// diagonals come out smooth rather than stepped.
fn push_run_button(
    list: &mut DrawList,
    at: ([f32; 2], [f32; 2]),
    busy: bool,
    rotation: f32,
    opacity: f32,
    theme: &Theme,
    hairline: f32,
) {
    let (origin, extent) = at;
    let side = extent[0].min(extent[1]);
    if side <= 0.0 {
        return;
    }
    list.push_quad(
        QuadInstance::solid(origin, extent, if busy { theme.accent } else { theme.surface })
            .with_corner_radius(side * 0.5)
            .with_border(if busy { theme.accent } else { theme.border }, hairline)
            .with_rotation(rotation)
            .with_opacity(opacity),
    );

    let ink = if busy { theme.surface } else { theme.accent };
    let centre = [origin[0] + extent[0] * 0.5, origin[1] + extent[1] * 0.5];
    if busy {
        // Stop: a square, which a quad expresses exactly and a mesh would only complicate.
        let bar = side * 0.32;
        list.push_quad(
            QuadInstance::solid([centre[0] - bar / 2.0, centre[1] - bar / 2.0], [bar, bar], ink)
                .with_corner_radius(bar * 0.15)
                .with_rotation(rotation)
                .with_opacity(opacity),
        );
        return;
    }

    let reach = side * 0.22;
    let nudge = reach * 0.18;
    // **Relative to the centre, because the transform below translates by it.** Spelling these
    // absolutely is what flung `push_play_button`'s triangle to twice its own offset, which the
    // user photographed as a stray play mark floating above a card.
    let tip = [reach + nudge, 0.0];
    let top = [-reach + nudge, -reach];
    let bottom = [-reach + nudge, reach];
    let transform =
        list.meshes_mut().push_transform(MeshTransform::scale_rotate_at(1.0, rotation, centre));
    let first = list.meshes().indices().len() as u32;
    list.meshes_mut().push_indexed(
        &[top, tip, bottom],
        &[0, 1, 2],
        ink.with_alpha(ink.a * opacity),
        transform,
    );
    let last = list.meshes().indices().len() as u32;
    list.push_meshes(first..last);
}

/// The browser's reload control: a broken ring with a head on it.
///
/// `↻` is U+21BB and outside the plain sans faces (trap 10), so the mark is drawn — a
/// three-quarter ring, made by laying a plate the colour of the node over the middle of a
/// filled circle, plus a small triangle for the arrow's head. Cheaper than tessellating an
/// annulus, and every piece of it is a quad or one triangle.
fn push_reload_button(
    list: &mut DrawList,
    at: ([f32; 2], [f32; 2]),
    rotation: f32,
    opacity: f32,
    theme: &Theme,
    hairline: f32,
) {
    let (origin, extent) = at;
    let side = extent[0].min(extent[1]);
    if side <= 0.0 {
        return;
    }
    let ring = side * 0.62;
    let weight = (side * 0.11).max(hairline);
    let centre = [origin[0] + extent[0] * 0.5, origin[1] + extent[1] * 0.5];
    list.push_quad(
        QuadInstance::solid(
            [centre[0] - ring / 2.0, centre[1] - ring / 2.0],
            [ring, ring],
            Rgba::TRANSPARENT,
        )
        .with_border(theme.text_muted, weight)
        .with_corner_radius(ring * 0.5)
        .with_rotation(rotation)
        .with_opacity(opacity),
    );
    // The gap in the ring, and the head sitting in it. The gap is painted in the node's own
    // surface rather than left out, because a `QuadInstance` border is a whole ring.
    let gap = weight * 2.4;
    list.push_quad(
        QuadInstance::solid(
            [centre[0] + ring / 2.0 - weight, centre[1] - gap / 2.0],
            [weight * 2.0, gap],
            theme.surface,
        )
        .with_rotation(rotation)
        .with_opacity(opacity),
    );
    let head = weight * 1.6;
    let transform = list.meshes_mut().push_transform(MeshTransform::scale_rotate_at(
        1.0,
        rotation,
        [centre[0] + ring / 2.0, centre[1]],
    ));
    let first = list.meshes().indices().len() as u32;
    list.meshes_mut().push_indexed(
        &[[-head, -head], [head, -head], [0.0, head * 0.6]],
        &[0, 1, 2],
        theme.text_muted.with_alpha(theme.text_muted.a * opacity),
        transform,
    );
    let last = list.meshes().indices().len() as u32;
    list.push_meshes(first..last);
}

/// A file tree's disclosure triangle: pointing right when shut, down when open.
///
/// Geometry rather than `▸`/`▾`, which are U+25B8 and U+25BE and outside the plain sans faces
/// this application can rely on (trap 10).
fn push_twisty(
    list: &mut DrawList,
    at: ([f32; 2], [f32; 2]),
    expanded: bool,
    rotation: f32,
    opacity: f32,
    theme: &Theme,
) {
    let (origin, extent) = at;
    let side = extent[0].min(extent[1]);
    if side <= 0.0 {
        return;
    }
    let centre = [origin[0] + extent[0] * 0.5, origin[1] + extent[1] * 0.5];
    let reach = side * 0.28;
    let points = if expanded {
        [[-reach, -reach * 0.6], [reach, -reach * 0.6], [0.0, reach * 0.8]]
    } else {
        [[-reach * 0.6, -reach], [-reach * 0.6, reach], [reach * 0.8, 0.0]]
    };
    let ink = theme.text_muted;
    let transform =
        list.meshes_mut().push_transform(MeshTransform::scale_rotate_at(1.0, rotation, centre));
    let first = list.meshes().indices().len() as u32;
    list.meshes_mut().push_indexed(
        &points,
        &[0, 1, 2],
        ink.with_alpha(ink.a * opacity),
        transform,
    );
    let last = list.meshes().indices().len() as u32;
    list.push_meshes(first..last);
}

/// The stripe a node too small for its own header wears instead of a status dot.
///
/// `AgentLayout` reports no `status` rectangle at that size — it reports none of its controls —
/// so a dot here would be a position this file invented, which is the one thing the shared
/// layout exists to forbid. A stripe down the node's own left edge is derived from `bounds` and
/// says the only thing there is room to say.
fn push_compact_stripe(
    list: &mut DrawList,
    position: [f32; 2],
    size: [f32; 2],
    colour: Rgba,
    rotation: f32,
    opacity: f32,
) {
    let width = (size[0] * 0.06).clamp(0.5, size[0].max(0.5));
    list.push_quad(
        QuadInstance::solid(position, [width, size[1]], colour)
            .with_corner_radius(width * 0.5)
            .with_rotation(rotation)
            .with_opacity(opacity),
    );
}

/// The card every Agent Canvas node is built on.
#[expect(
    clippy::too_many_arguments,
    reason = "one card, and every caller passes the same facts the four kinds share"
)]
fn push_node_card(
    list: &mut DrawList,
    position: [f32; 2],
    size: [f32; 2],
    fill: Rgba,
    rotation: f32,
    opacity: f32,
    theme: &Theme,
    hairline: f32,
) {
    list.push_quad(
        QuadInstance::solid(position, size, fill)
            .with_corner_radius(CARD_RADIUS)
            .with_border(theme.border, hairline)
            .with_rotation(rotation)
            .with_opacity(opacity),
    );
}

/// The body a note node draws.
///
/// **Not wired yet, and this is the whole of the join.** A note's content is a `.md` file on
/// disk (`docs/07` §8) and nothing on the paint path may touch a filesystem, so the body has to
/// arrive the way an agent's transcript does — read once per frame by the runtime and handed to
/// the painter as a snapshot. Until `crate::agent_view` grows that map this answers `None`, and
/// `note_paint` draws a line saying so rather than an empty box.
fn note_body<'a>(ctx: &DrawContext<'a>, id: SceneId) -> Option<&'a str> {
    ctx.agents.note(id)
}

/// The rows a file tree draws.
///
/// The same join as [`note_body`], and the same reason: `vellum_agent::filetree::visible` reads
/// directories, which is not something a frame may do. Until the runtime supplies it this
/// answers `None` and the node says it has not been read.
fn tree_view<'a>(ctx: &DrawContext<'a>, id: SceneId) -> Option<&'a TreeView> {
    ctx.agents.tree(id)
}

/// Whether browser nodes are permitted at all.
///
/// The app's own preference, carried on `AgentViews` because the painter has no library to
/// ask. `docs/07` §0 rule 3 has browser nodes off by default, so on a default installation
/// every node draws `placeholder_reason`'s *"Browser nodes are off — turn them on in
/// Preferences"*, which is exactly what it should say. (This used to add that the preference
/// was "not plumbed through to the painter yet". It is — `ActiveState::rebuild_agent_views`
/// sets it every frame.)
const fn browser_nodes_enabled(ctx: &DrawContext<'_>) -> bool {
    ctx.agents.browser_nodes()
}

/// What an Agent Canvas node's pieces were built against, so a frame can tell whether it still
/// holds the right ones.
#[derive(Debug)]
struct CachedNode {
    /// The frame counter [`Painter::paint`] bumps.
    ///
    /// **This is what makes a live transcript redraw**, and it is why the entry is not keyed on
    /// the projection's generation the way every other cache in this file is: an agent's output
    /// is not in the document, so nothing about the document moves when it changes.
    frame: u64,
    generation: u64,
    size: (f64, f64),
    /// A coarse content signature, so a caller that does **not** go through `paint` — a test
    /// calling `block` twice with a changed view — still sees the change. The frame counter is
    /// the real invalidation; this is the guard for the paths that never bump it.
    signature: u64,
    paint: NodePaint,
}

/// A cheap signature over what a node draws.
///
/// Deliberately coarse: it is asked once per `block` call, so hashing every transcript event
/// here would be quadratic in the run count. The frame counter beside it in [`CachedNode`] is
/// what actually forces a rebuild; this only has to catch a change that happens without one.
fn node_signature(kind: &ItemKind, view: Option<&AgentView>) -> u64 {
    let token = match kind {
        ItemKind::Agent { model, .. }
        | ItemKind::AgentNote { model, .. }
        | ItemKind::FileTree { model }
        | ItemKind::Browser { model } => model.as_str(),
        _ => "",
    };
    let mut hash = fnv1a(token.as_bytes(), 0);
    if let Some(view) = view {
        hash = fnv1a(view.detail.as_bytes(), hash);
        hash = fnv1a(view.draft.as_bytes(), hash);
        hash = fnv1a(view.subtitle.as_bytes(), hash);
        hash = fnv1a(
            &[
                view.status as u8,
                u8::from(view.truncated),
                u8::from(matches!(view.mode, DisplayMode::Raw)),
            ],
            hash,
        );
        // **Whether the prompt row has the keyboard, not just what is in it.** The draft above
        // covers the characters; this covers the *state change* — arriving in the row with an
        // empty buffer, or leaving it — which alters the run without altering a byte of text,
        // and would otherwise draw a placeholder where the caret is.
        hash = fnv1a(&[u8::from(view.caret.is_some())], hash);
        // ⚠ **And the voice line, which changes once a second while nothing else does.** The
        // elapsed count is the only thing on a listening node that moves, so a signature blind
        // to it leaves the node reading *"Listening… 0s"* for the whole of a press on any path
        // that does not bump the frame counter. Hashed as the drawn string rather than as a
        // discriminant, because that string is what is on screen — the same reason `detail` is
        // hashed above and not the status it was derived from.
        if let Some(spoken) = view.voice.as_ref().map(voice_line) {
            hash = fnv1a(spoken.as_bytes(), hash);
        }
        hash = fnv1a(&view.events.len().to_le_bytes(), hash);
        // The newest entry is the one that streams, and it is the one whose *identity* the rest
        // of this signature cannot see: a tail that gained an event and lost one off the front
        // has the same length it had.
        if let Some(last) = view.events.last() {
            hash = fnv1a(last.headline().as_bytes(), hash);
        }
    }
    hash
}

/// The pieces one Agent Canvas node draws, laid out for its kind.
fn build_node_paint(
    kind: &ItemKind,
    view: Option<&AgentView>,
    ctx: &DrawContext<'_>,
    id: SceneId,
    size: (f64, f64),
) -> NodePaint {
    let (width, height) = size;
    let font = node_font_size(width);
    match kind {
        ItemKind::Agent { model, .. } => {
            // A node with no view is one the runtime has not attached a session to yet. It
            // still knows what it is *configured* to run on, from its own token — so it draws
            // its provider line rather than nothing, and the moment a session appears the live
            // detail replaces it in the same place.
            let fallback;
            let view = match view {
                Some(view) => view,
                None => {
                    fallback = AgentView {
                        subtitle: crate::agent::subtitle(&crate::agent::decode(model)),
                        // The same resolution the toggle above makes, so a node with no
                        // session yet paints its transcript area and its badge in one mode.
                        mode: crate::agent::display_mode(
                            &crate::agent::decode(model),
                            ctx.default_display,
                        ),
                        ..AgentView::default()
                    };
                    &fallback
                }
            };
            agent_paint(view, &crate::agent::layout(width, height), font)
        }
        ItemKind::AgentNote { model, .. } => note_paint(
            &crate::note::decode(model),
            note_body(ctx, id),
            &crate::note::layout(width, height),
            font,
        ),
        ItemKind::FileTree { model } => tree_paint(
            &crate::filetree::decode(model),
            tree_view(ctx, id),
            &crate::filetree::layout(width, height),
            font,
            ctx.agents.tree_scroll(id),
        ),
        ItemKind::Browser { model } => {
            let browser = crate::browser::decode(model);
            let enabled = browser_nodes_enabled(ctx);
            browser_paint(
                &browser,
                // The configuration **and** the pool's own word for this page. Asking only
                // `placeholder_reason` — which is what this did — leaves a node whose page
                // would not load, or which has been panned half off the canvas, drawing
                // nothing under its own address to say so.
                crate::browser::viewport_message(&browser, enabled, ctx.agents.browser_note(id)),
                &crate::browser::layout(width, height),
                font,
            )
        }
        _ => NodePaint::default(),
    }
}

/// Where an agent's role, or a note's title, is drawn.
///
/// Both are the item's own `StyledText` and both go through `BlockKey::PRIMARY`, so the box
/// comes from the node's own layout rather than from a rule invented in `block`. The role takes
/// only the *top* line of its rectangle — `agent_paint` places the subtitle from the same
/// rectangle's foot, and the two have to be derived from one number or they overlap.
fn node_title_box(kind: &ItemKind, size: (f64, f64), font: f64) -> (f64, f64, f64, f64) {
    let (w, h) = size;
    match kind {
        ItemKind::Agent { .. } => {
            let laid = crate::agent::layout(w, h);
            let rect = laid.role;
            let height = if laid.too_small {
                rect.height
            } else {
                (font * ROLE_SCALE * NODE_LINE_HEIGHT).min(rect.height)
            };
            (rect.x, rect.y, rect.width, height)
        }
        ItemKind::AgentNote { .. } => {
            let rect = crate::note::layout(w, h).title;
            (rect.x, rect.y, rect.width, rect.height)
        }
        // A file tree and a browser carry no `StyledText` at all — `ItemKind::text` says so —
        // so nothing ever asks this for one.
        _ => (0.0, 0.0, w, h),
    }
}

/// Whether a kind is one of the four the Agent Canvas draws.
const fn is_node(kind: &ItemKind) -> bool {
    matches!(
        kind,
        ItemKind::Agent { .. }
            | ItemKind::AgentNote { .. }
            | ItemKind::FileTree { .. }
            | ItemKind::Browser { .. }
    )
}

/// The weight an agent connector is drawn at, in **device** pixels.
///
/// Screen-constant, which is a change of category from an ordinary connector and is the point:
/// an agent link is not ink somebody drew on the board, it is a statement about what two nodes
/// may do to each other. Drawing it at a constant screen weight puts it in the same family as
/// the selection ring, a frame's hairline and the alignment guides — and it is what makes the
/// **dash cadence** screen-constant too, because `vellum_connect` derives the pattern from the
/// thickness. A world-unit cadence is a solid smear at 4% and three dashes across the window at
/// 8×, which is the failure `push_grid` and `push_dashed` both already record.
const AGENT_LINK_WIDTH: f64 = 1.6;

/// The most dashes one agent link is allowed to cost.
///
/// The same backstop `GUIDE_MAX_DASHES` is, arrived at by the same arithmetic: a connector
/// between two agents at opposite ends of a board is mostly off screen at a working zoom, and
/// `vellum_connect` dashes the whole routed path because it has no viewport to clip against.
/// Past the cap the link draws **solid** rather than at a coarser cadence — it is still the
/// link's colour and still legible as an agent link, and a rhythm stretched until it fits is a
/// rhythm that no longer says anything.
const AGENT_LINK_MAX_DASHES: f64 = 512.0;

/// The dot that travels a link while a message is passing along it, in device pixels.
const PULSE_DIAMETER: f64 = 7.0;

/// How the two kinds of agent link are told apart.
///
/// A **message** link is dashed and a **context** link is dotted — `vellum_connect`'s own
/// vocabulary rather than a pattern invented here, and far enough apart to read at a glance,
/// which is the requirement: *"reads this" and "talks to this" are not the same relationship
/// and a board where they look alike is one you cannot follow*.
///
/// The colour splits them a second time. A message link wears the accent, because it is the
/// live one — a pulse travels it. A context link is muted: it describes something standing
/// rather than something happening.
fn agent_link_look(
    link: crate::agent::LinkKind,
    theme: &Theme,
) -> Option<(vellum_connect::LineStyle, Rgba)> {
    match link {
        crate::agent::LinkKind::Plain => None,
        crate::agent::LinkKind::Message(_) => {
            Some((vellum_connect::LineStyle::Dashed, theme.accent))
        }
        crate::agent::LinkKind::Context => {
            Some((vellum_connect::LineStyle::Dotted, theme.text_muted))
        }
    }
}

/// The point a fraction `t` of the way along a polyline, by arc length.
///
/// `None` for a polyline with no points at all. A zero-length one answers its own only point,
/// which is the degenerate connector: the pulse sits on it rather than being placed at an
/// arbitrary end.
fn point_along(line: &vellum_connect::Polyline, t: f64) -> Option<vellum_connect::Point> {
    let total = line.length();
    if total <= f64::EPSILON || line.points.len() < 2 {
        return line.points.first().copied();
    }
    let target = t.clamp(0.0, 1.0) * total;
    let mut travelled = 0.0;
    for pair in line.points.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        let span = a.distance_to(b);
        if span <= f64::EPSILON {
            continue;
        }
        if travelled + span >= target {
            let along = (target - travelled) / span;
            return Some(vellum_connect::Point::new(
                a.x + (b.x - a.x) * along,
                a.y + (b.y - a.y) * along,
            ));
        }
        travelled += span;
    }
    line.points.last().copied()
}

/// The message travelling a link right now.
///
/// # Why this is not the caret blink this application refuses to have
///
/// A blink needs a timer driving redraws while nothing is happening. This asks for repaints for
/// the fraction of a second a message is actually in flight and then stops — the board
/// library's `landed_background` rule (feedback 21), not the thing that rule is contrasted
/// with. `AgentViews::any_in_flight` is the early-out that makes an idle board cost one
/// boolean, and it is the **app's frame loop** that has to consult it: nothing in the painter
/// can ask for the next frame, so a build that never calls it draws a pulse frozen at whatever
/// progress the frame it appeared on happened to carry.
fn push_link_pulse(
    list: &mut DrawList,
    ctx: &DrawContext<'_>,
    line: &vellum_connect::Polyline,
    pulse: vellum_agent::LinkPulse,
    colour: Rgba,
) {
    let progress = f64::from(pulse.progress).clamp(0.0, 1.0);
    // `forward` is start → end, which is the direction the path was built in — so a backward
    // message is the same journey walked from the other end rather than a second code path.
    let t = if pulse.forward { progress } else { 1.0 - progress };
    let Some(at) = point_along(line, t) else { return };

    let diameter = (PULSE_DIAMETER / ctx.camera.zoom()) as f32;
    let origin = ctx.camera.to_camera_relative(WorldPoint::new(at.x, at.y));
    // A short tail behind the dot, so a *still* frame says which way it is going — a
    // `--screenshot` catches one instant and a dot alone at that instant is directionless. Two
    // quads rather than a gradient: the mark is seven pixels across and a gradient at that size
    // is one more pipeline for something nobody can resolve.
    let tail = if pulse.forward { t - 0.06 } else { t + 0.06 };
    if let Some(behind) = point_along(line, tail.clamp(0.0, 1.0)) {
        let small = diameter * 0.55;
        let back = ctx.camera.to_camera_relative(WorldPoint::new(behind.x, behind.y));
        list.push_quad(
            QuadInstance::solid(
                [back[0] - small / 2.0, back[1] - small / 2.0],
                [small, small],
                colour.with_alpha(colour.a * 0.4),
            )
            .with_corner_radius(small / 2.0),
        );
    }
    list.push_quad(
        QuadInstance::solid(
            [origin[0] - diameter / 2.0, origin[1] - diameter / 2.0],
            [diameter, diameter],
            colour,
        )
        .with_corner_radius(diameter / 2.0),
    );
}

/// A card body: a rounded rectangle drawn through the SDF pipeline.
///
/// Deliberately the analytic path rather than the quad one. It is the same shape
/// either way today, and routing cards through `vellum_shapes` means the day
/// `ItemKind` grows a shape variant the plumbing — parameters, arena, instance — is
/// already exercised on a real board rather than only in a unit test.
fn push_shape_card(
    list: &mut DrawList,
    position: [f32; 2],
    size: [f32; 2],
    rotation: f32,
    opacity: f32,
    theme: &Theme,
) {
    // `Shape::RoundedRectangle`'s radius is a fraction of the **shorter** side, which
    // is what keeps the corners circular however the card is stretched.
    let radius = (CARD_RADIUS / size[0].min(size[1]).max(1.0)).clamp(0.0, 0.5);
    let shape = Shape::RoundedRectangle { radius };
    let Some(params) = shape.sdf_params(Size::new(size[0], size[1])) else {
        return;
    };
    let style = ShapeStyle {
        fill: theme.surface,
        border: theme.border,
        border_width: HAIRLINE,
        rotation,
        opacity,
    };
    list.push_shape(
        &params,
        [position[0] + size[0] / 2.0, position[1] + size[1] / 2.0],
        &style,
    );
}

/// What an image draws before its pixels arrive: the card surface plus a hairline,
/// so the item's place on the board is visible immediately rather than as a hole.
fn push_placeholder(
    list: &mut DrawList,
    position: [f32; 2],
    size: [f32; 2],
    rotation: f32,
    opacity: f32,
    theme: &Theme,
) {
    list.push_quad(
        QuadInstance::solid(position, size, theme.surface)
            .with_border(theme.border, HAIRLINE)
            .with_rotation(rotation)
            .with_opacity(opacity),
    );
}

/// The pen stroke in flight, drawn exactly as the committed one will be.
///
/// Tessellated fresh every frame rather than cached. The point list grows with every
/// pointer sample, so a cache would miss on each one anyway — and the cache the
/// committed strokes use keys on [`Projection::generation`], which deliberately does
/// not move for anything the document has not seen yet.
fn push_stroke(list: &mut DrawList, ctx: &DrawContext<'_>, board: u32) {
    let Some(stroke) = ctx.stroke else { return };
    // One sample is a press that has not travelled, not a mark. `commit_stroke`
    // applies the same floor, so nothing is previewed that would not be kept.
    if stroke.points.len() < 2 {
        return;
    }

    // Tessellated about the stroke's own first point rather than the board origin.
    // `vellum_ink::Mesh` holds f32 positions and is only safe in stroke-local space —
    // a stroke is a few hundred units across, a board is tens of thousands — so the
    // offset stays in f64 here and reaches the GPU through the transform, which is
    // the same division of labour the committed path and the connectors use.
    let origin = stroke.points[0];
    let relative: Vec<vellum_doc::Point> = stroke
        .points
        .iter()
        .map(|point| vellum_doc::Point { x: point.x - origin.x, y: point.y - origin.y })
        .collect();
    let mesh = tessellate_ink(&relative, stroke.thickness, lod_band(ctx.camera.zoom()));

    list.use_view(board);
    let transform = list
        .meshes_mut()
        .push_transform(MeshTransform::at(ctx.camera.to_camera_relative(origin)));
    let start = list.meshes().indices().len() as u32;
    list.meshes_mut().push_ink(&mesh, stroke.color, transform);
    let end = list.meshes().indices().len() as u32;
    list.push_meshes(start..end);
}

/// The marquee rectangle, in screen pixels so it stays crisp at any zoom.
/// The device-pixel width of the caret, and of a selection edge.
///
/// One logical pixel scaled by the display, not by the camera: a caret is chrome, and one
/// that fattened with the zoom would look like a highlighted column rather than a caret.
const CARET_WIDTH: f32 = 2.0;

/// The caret, in the screen view where the glyphs are.
///
/// Screen view rather than board view because the caret has to line up with glyphs, and
/// those are rasterised at their drawn size and pushed at device coordinates. `Caret`'s
/// numbers are block-relative *logical* px — the same space the layout's glyph positions
/// are in — so the camera's zoom is the only conversion needed.
/// How long the caret stays solid after a keystroke before it starts blinking.
///
/// Every text field on both platforms does this: a caret that blinked *while* you typed
/// would flicker under your own hands, and the blink is there to be found when you stop.
const CARET_SOLID_FOR: f32 = 0.5;

/// One full on-then-off cycle. macOS's own rate.
const CARET_BLINK_PERIOD: f32 = 1.06;

/// Whether the caret is drawn this frame.
///
/// *"i want there to be appearing and disappearing this sign | that indicates that i
/// started typing"*. `CLAUDE.md` recorded no-blink as deliberate — *"a blink needs a timer
/// driving redraws while nothing is happening, which is what this app exists not to do"* —
/// and the reasoning does not apply here: the loop already presents every frame while the
/// window is visible, and a caret only exists while a session is open, which is an active
/// state by definition. Nothing new is woken up.
///
/// Pure, and a function rather than an expression inline, so the phase is testable without
/// a window.
fn caret_is_visible(idle_for: f32) -> bool {
    if idle_for < CARET_SOLID_FOR {
        return true;
    }
    (idle_for - CARET_SOLID_FOR) % CARET_BLINK_PERIOD < CARET_BLINK_PERIOD / 2.0
}

/// The prompt row's caret, as the same [`TextCursor`] the on-canvas caret uses.
///
/// Free rather than a method because it borrows nothing but the frame's context: the string is
/// `AgentView::draft`, which is the buffer's own — `ActiveState::show_live_prompt` puts it
/// there every frame precisely so the offsets and the characters they index arrive together.
///
/// `None` unless `slot` is the run the prompt was drawn into, which
/// [`NodePaint::prompt_slot`] answered from the paint the painter just built. Deriving the
/// slot here instead would be a second opinion about which run holds the prompt, which is the
/// `kanban_runs` failure — a caret drawn on one row while the typing goes into another.
fn prompt_cursor<'a>(
    id: SceneId,
    slot: u16,
    prompt_slot: Option<u16>,
    ctx: &DrawContext<'a>,
) -> Option<TextCursor<'a>> {
    if prompt_slot != Some(slot) {
        return None;
    }
    // Bound out of the context first, so the borrow is the views' own `'a` and not a reborrow
    // limited to this `&DrawContext` — the string has to outlive the call.
    let views: &'a crate::agent_view::AgentViews = ctx.agents;
    let view = views.get(id)?;
    let caret = view.caret?;
    Some(TextCursor {
        scene: id,
        slot,
        idle_for: caret.idle_for,
        cursor: caret.cursor,
        anchor: caret.anchor,
        text: view.draft.as_str(),
    })
}

fn push_caret(
    list: &mut DrawList,
    ctx: &DrawContext<'_>,
    block: &Block,
    layout: &vellum_text::Layout,
    cursor: TextCursor<'_>,
) {
    if !caret_is_visible(cursor.idle_for) {
        return;
    }
    let caret = layout.caret(cursor.text, cursor.cursor);
    // **The block's own scale, not the camera's.** They differ for a frame title,
    // which is caret-editable and refuses to shrink past legibility — so below the
    // floor the glyphs draw magnified and a caret measured off the camera would sit
    // at the unmagnified position, which is the paint/press disagreement this file
    // hunts. `Block::scale` is the camera's zoom for every other kind.
    let zoom = block.scale;
    let origin = [
        block.origin.x as f32 + caret.x * zoom,
        block.origin.y as f32 + caret.top * zoom,
    ];
    list.push_quad(QuadInstance::solid(
        origin,
        [CARET_WIDTH, (caret.height * zoom).max(CARET_WIDTH)],
        ctx.theme.accent,
    ));
}

/// The selection highlight, one quad per visual line, under the glyphs.
fn push_selection_boxes(
    list: &mut DrawList,
    ctx: &DrawContext<'_>,
    block: &Block,
    layout: &vellum_text::Layout,
    cursor: TextCursor<'_>,
) {
    if cursor.cursor == cursor.anchor {
        return;
    }
    let range = cursor.cursor.min(cursor.anchor)..cursor.cursor.max(cursor.anchor);
    // **The block's own scale, not the camera's.** They differ for a frame title,
    // which is caret-editable and refuses to shrink past legibility — so below the
    // floor the glyphs draw magnified and a caret measured off the camera would sit
    // at the unmagnified position, which is the paint/press disagreement this file
    // hunts. `Block::scale` is the camera's zoom for every other kind.
    let zoom = block.scale;
    for box_ in layout.selection_boxes(cursor.text, range) {
        list.push_quad(QuadInstance::solid(
            [
                block.origin.x as f32 + box_.x * zoom,
                block.origin.y as f32 + box_.top * zoom,
            ],
            [(box_.width * zoom).max(1.0), (box_.height * zoom).max(1.0)],
            // Light enough that the glyphs on top of it still meet contrast, which is the
            // rule `docs/05-design-language.md` §3a states for glass and which applies
            // just as much here: legibility wins over the marker.
            ctx.theme.accent.with_alpha(0.22),
        ));
    }
}

/// The connector being drawn, as a straight accent line with a blob at each end.
///
/// Straight rather than routed: the routing mode is a property of the finished item and the
/// router needs both endpoints resolved against their targets, which is not decided until
/// the button comes up. A straight line is honest about that — it shows where the two ends
/// are, which is the only thing in question mid-drag.
fn push_pending_connector(list: &mut DrawList, ctx: &DrawContext<'_>, board: u32) {
    let Some((from, to)) = ctx.pending_connector else { return };
    list.use_view(board);
    let camera = ctx.camera;
    let a = camera.to_camera_relative(from);
    let b = camera.to_camera_relative(to);
    // World units, so the line keeps a constant thickness on screen as the camera zooms —
    // the same division the selection ring and a frame's hairline already do.
    let width = (f64::from(SELECTION_WIDTH) * 1.5 / camera.zoom()) as f32;
    let mesh = crate::mesh::ribbon(&[a, b], width.max(0.01));
    push_local_mesh(list, [0.0, 0.0], &mesh, ctx.theme.accent, 0.0);

    // A blob at each end, so it is visible where the line is going to attach even when the
    // drag is only a few pixels long and the ribbon is a smear.
    let blob = width * 2.5;
    for at in [a, b] {
        list.push_quad(
            QuadInstance::solid([at[0] - blob / 2.0, at[1] - blob / 2.0], [blob, blob], ctx.theme.accent)
                .with_corner_radius(blob / 2.0),
        );
    }
}

/// The item a create tool is sweeping out, drawn as it will look when the button comes up.
///
/// Board view, like the pen's live stroke and the kanban card's placeholder: it is a
/// rectangle in the board's own space, and a camera that moved mid-gesture would leave a
/// screen-space preview behind.
///
/// Drawn **before** the selection ring and after the items, so it sits on the board like
/// the thing it is previewing rather than over the chrome.
fn push_placing(list: &mut DrawList, ctx: &DrawContext<'_>, board: u32) {
    let Some(placing) = ctx.placing else { return };
    list.use_view(board);
    let camera = ctx.camera;
    let theme = ctx.theme;

    // A `Placement` is a **centre** and an extent, so the top-left is derived rather than
    // read. Through `project::placement_bounds`, which is the function every item on the
    // board is already positioned by — reading `x`/`y` as a top-left draws the preview
    // half its own size up and to the left of where the item lands, and at a placing
    // tool's default sizes that is most of a screen.
    let rect = crate::project::placement_bounds(&placing.placement);
    let origin = camera.to_camera_relative(rect.min);
    let size = [(rect.max.x - rect.min.x) as f32, (rect.max.y - rect.min.y) as f32];
    // One device pixel at any zoom, like the selection ring and a frame's own edge: a
    // preview outline that fattened as the board was zoomed in would stop reading as an
    // edge and start reading as part of the item.
    let hairline = (f64::from(HAIRLINE) / camera.zoom()) as f32;

    match placing.look {
        PlacingLook::Frame => {
            list.push_quad(
                QuadInstance::solid(origin, size, theme.frame_fill)
                    .with_border(theme.border, hairline),
            );
        }
        PlacingLook::Sticky => {
            list.push_quad(
                QuadInstance::solid(origin, size, theme.sticky)
                    .with_corner_radius(STICKY_RADIUS),
            );
        }
        // The same surface, radius and hairline the placed node draws with, so the preview
        // and the item are one picture rather than two that happen to agree.
        PlacingLook::Card => {
            list.push_quad(
                QuadInstance::solid(origin, size, theme.surface)
                    .with_corner_radius(CARD_RADIUS)
                    .with_border(theme.border, hairline),
            );
        }
        PlacingLook::Shape(shape) => {
            let extent = Size::new(size[0], size[1]);
            let centre = [origin[0] + size[0] / 2.0, origin[1] + size[1] / 2.0];
            let style = ShapeStyle {
                fill: theme.surface,
                border: theme.border,
                border_width: hairline,
                rotation: 0.0,
                opacity: 1.0,
            };
            // Analytic where the form allows it, tessellated where it does not — the same
            // two paths the placed shape takes, chosen the same way. A form with neither
            // is drawn as a ghost rather than skipped: an empty preview is the bug being
            // fixed.
            if let Some(params) = shape.sdf_params(extent) {
                list.push_shape(&params, centre, &style);
            } else {
                push_ghost(list, &theme, origin, size, hairline);
            }
        }
        PlacingLook::Ghost => push_ghost(list, &theme, origin, size, hairline),
    }

    // An accent outline on top of every one of them, at the marquee's weight. Two jobs:
    // it says *this is a gesture, not an item yet* — a bare white rectangle mid-drag is
    // indistinguishable from a frame that has already been placed — and it keeps a
    // sticky's own fill from being the only thing on screen when the sweep is a few
    // pixels across.
    list.push_quad(
        QuadInstance::solid(origin, size, Rgba::TRANSPARENT)
            .with_border(theme.accent, (f64::from(SELECTION_WIDTH) / camera.zoom()) as f32),
    );
}

/// Miro's alignment guides: the lines that say *this lines up with that*.
///
/// **Screen view**, unlike almost everything else a gesture draws. A guide is chrome — it
/// describes the board rather than being part of it — so its weight has to be one device
/// pixel at every zoom, and the segment ends and tick marks of a spacing hint have to be a
/// readable size whatever the board's scale is. In the board view all three would shrink
/// with the zoom and a guide at 4% would be invisible.
///
/// The **span** matters as much as the line. Miro draws a guide only across the items it
/// joins, and so does this: a line from edge to edge of the screen says *something over
/// there lines up*, which is a weaker and much less useful statement than *these two do*.
fn push_guides(list: &mut DrawList, ctx: &DrawContext<'_>, screen: u32) {
    if ctx.guides.is_empty() {
        return;
    }
    list.use_view(screen);
    let camera = ctx.camera;
    // **Dashed and translucent**, and both halves are the owner's call: *"they are too
    // bright, so make those into dotted dashed lines and turn transparency down a bit so i
    // can see the difference between the alignment line and an object"*.
    //
    // The complaint is exact and it is about *category*, not taste. A solid accent hairline
    // is what a selection ring is and what a shape's border is — so a guide drawn that way
    // reads as an edge belonging to something, and on a board of stickies it is one more
    // line among many. A dash belongs to no object: nothing else on the canvas is dashed
    // except a connector that was asked to be. The alpha then puts it behind the board
    // rather than on it.
    let colour = ctx.theme.accent.with_alpha(GUIDE_ALPHA);
    // A hairline, and a hair over one pixel so it survives the rounding either way.
    let weight = HAIRLINE.max(1.0);

    for guide in ctx.guides {
        // Both ends in screen space, so the guide is drawn from the same two points the
        // board is — no separate scaling to get wrong.
        let (a, b) = match guide.axis {
            crate::snap::Axis::Vertical => (
                camera.world_to_screen(WorldPoint::new(guide.at, guide.from)),
                camera.world_to_screen(WorldPoint::new(guide.at, guide.to)),
            ),
            crate::snap::Axis::Horizontal => (
                camera.world_to_screen(WorldPoint::new(guide.from, guide.at)),
                camera.world_to_screen(WorldPoint::new(guide.to, guide.at)),
            ),
        };
        push_dashed(list, ctx, guide.axis, a, b, colour, weight);

        // An equal-spacing hint is a *measurement*, so it gets a tick at each end — the
        // difference between "these are the same distance apart" and "this line runs
        // through both of them". Without them a gap hint is indistinguishable from an
        // alignment guide that happens to be short.
        //
        // The caps stay **solid**: they are four pixels long, and a dash pattern at that
        // size is a dot. They are what says the dashed line between them is a span.
        if guide.gap.is_some() {
            let tick = GUIDE_TICK;
            for end in [a, b] {
                let (tx, ty, tw, th) = match guide.axis {
                    crate::snap::Axis::Vertical => {
                        (end.x as f32 - tick, end.y as f32, tick * 2.0, weight)
                    }
                    crate::snap::Axis::Horizontal => {
                        (end.x as f32, end.y as f32 - tick, weight, tick * 2.0)
                    }
                };
                list.push_quad(QuadInstance::solid([tx, ty], [tw, th], colour));
            }
        }
    }
}

/// One guide line, as a run of dashes in **screen** pixels.
///
/// Clipped to the viewport first, and that is not an optimisation. A guide spans the items
/// it joins, and those are in *world* units — two stickies a hundred thousand units apart on
/// a zoomed-in board produce a segment whose on-screen length is enormous and almost
/// entirely off screen. Dashing that unclipped is a quad per six pixels of a line nobody can
/// see, sixty times a second. [`GUIDE_MAX_DASHES`] is the backstop for whatever this misses.
fn push_dashed(
    list: &mut DrawList,
    ctx: &DrawContext<'_>,
    axis: crate::snap::Axis,
    a: ScreenPoint,
    b: ScreenPoint,
    colour: Rgba,
    weight: f32,
) {
    let viewport = ctx.camera.viewport();
    let (limit_min, limit_max) = match axis {
        // A little past the edge on each side, so a dash is never cut in a way that reads
        // as the line stopping short of the window.
        crate::snap::Axis::Vertical => (-GUIDE_DASH, viewport.height as f32 + GUIDE_DASH),
        crate::snap::Axis::Horizontal => (-GUIDE_DASH, viewport.width as f32 + GUIDE_DASH),
    };
    let (along_a, along_b, across) = match axis {
        crate::snap::Axis::Vertical => (a.y as f32, b.y as f32, a.x as f32),
        crate::snap::Axis::Horizontal => (a.x as f32, b.x as f32, a.y as f32),
    };
    let start = along_a.min(along_b).max(limit_min);
    let end = along_a.max(along_b).min(limit_max);
    if end <= start {
        return;
    }

    let step = GUIDE_DASH + GUIDE_GAP;
    let dashes = (((end - start) / step).ceil() as usize).min(GUIDE_MAX_DASHES);
    for index in 0..dashes {
        let from = start + index as f32 * step;
        let to = (from + GUIDE_DASH).min(end);
        if to <= from {
            break;
        }
        let (x, y, w, h) = match axis {
            crate::snap::Axis::Vertical => (across, from, weight, to - from),
            crate::snap::Axis::Horizontal => (from, across, to - from, weight),
        };
        list.push_quad(QuadInstance::solid([x, y], [w, h], colour));
    }
}

/// Half the length of the cap at each end of a spacing hint, in device pixels.
const GUIDE_TICK: f32 = 4.0;

/// How opaque a guide is against the board.
///
/// Low enough to read as chrome rather than as an edge belonging to something, high enough
/// to survive the pale canvas `docs/05` §1 whitened — *"turn transparency down a bit"*, and
/// *a bit* is the operative word. Under about a third the line disappears over a sticky.
const GUIDE_ALPHA: f32 = 0.45;

/// The dash and the gap between dashes, in device pixels.
///
/// Screen pixels, like the guide's weight, so the pattern is the same density at every zoom
/// — a world-unit dash would be a solid line when zoomed out and three dashes across the
/// window when zoomed in, which is the failure `push_grid` already documents for the board's
/// own dots.
const GUIDE_DASH: f32 = 5.0;
const GUIDE_GAP: f32 = 4.0;

/// A backstop on the dashes one guide may emit.
///
/// The viewport clip above should make this unreachable; it is here because the alternative
/// to being wrong about that is a frame that emits a hundred thousand quads and stops.
const GUIDE_MAX_DASHES: usize = 512;

/// The accent wash a preview falls back to when the finished item's look is not known.
fn push_ghost(
    list: &mut DrawList,
    theme: &Theme,
    origin: [f32; 2],
    size: [f32; 2],
    hairline: f32,
) {
    list.push_quad(
        QuadInstance::solid(origin, size, theme.accent.with_alpha(0.12))
            .with_border(theme.accent, hairline),
    );
}

/// The placeholder a dragged kanban card would drop into.
///
/// Board view, not screen view: it is a rectangle in the board's own space that has to
/// stay put under the pointer as the camera moves, and it may be rotated with its item.
/// Drawn as four edges rather than one quad because a rotated placeholder is not
/// axis-aligned and `QuadInstance` takes an origin and an extent.
fn push_card_drop(list: &mut DrawList, ctx: &DrawContext<'_>, board: u32) {
    let Some(corners) = ctx.card_drop else { return };
    list.use_view(board);
    let camera = ctx.camera;
    // Constant on screen at any zoom, at the gesture chrome's weight: a placeholder that
    // fattened as the board was zoomed in would stop reading as an insertion line.
    let width = (f64::from(SELECTION_WIDTH) / camera.zoom()) as f32;
    for pair in 0..4 {
        let from = corners[pair];
        let to = corners[(pair + 1) % 4];
        let a = camera.to_camera_relative(WorldPoint::new(from.0, from.1));
        let b = camera.to_camera_relative(WorldPoint::new(to.0, to.1));
        // Each edge as a thin quad along itself. Axis-aligned in the common case —
        // an unrotated board — and a stair-step of one pixel otherwise, which at a
        // placeholder's weight is not visible.
        let (x, y) = (a[0].min(b[0]), a[1].min(b[1]));
        let (w, h) = ((b[0] - a[0]).abs().max(width), (b[1] - a[1]).abs().max(width));
        list.push_quad(QuadInstance::solid([x, y], [w, h], ctx.theme.accent));
    }
}

fn push_marquee(list: &mut DrawList, ctx: &DrawContext<'_>, screen: u32) {
    let Some((from, to)) = ctx.marquee else { return };
    list.use_view(screen);
    let origin = [from.x.min(to.x) as f32, from.y.min(to.y) as f32];
    let size = [(to.x - from.x).abs() as f32, (to.y - from.y).abs() as f32];
    list.push_quad(
        QuadInstance::solid(origin, size, ctx.theme.accent.with_alpha(0.12))
            .with_border(ctx.theme.accent, SELECTION_WIDTH),
    );
}

/// The board's background pattern — dots, crosses or graph lines.
///
/// Drawn in [`Theme::grid`], **not** in `frost`/`border` as `docs/05-design-language.md`
/// §4 originally specified. Against `canvas` that is a 2.4% channel delta — a contrast
/// ratio of 1.05:1 — so a 2px dot rendered perfectly and could not be seen at all,
/// while lines survived only because a full-height bar lays down some thirty-five times
/// the ink per cell at the same colour. That asymmetry is the whole reason the two
/// patterns appeared to behave differently.
///
/// Dots rather than lines, and in **screen** space rather than world space. Both
/// follow from the same requirement: the grid has to stay a constant, quiet texture
/// at every zoom. A world-space grid would fatten into stripes as you zoomed in and
/// alias into moiré as you zoomed out, and lines at any zoom read as graph paper
/// rather than as a datum field.
///
/// The spacing is the world step whose on-screen size lands inside a comfortable
/// band, chosen from the 1-2-5 decade sequence a technical drawing would use, so
/// zooming steps the grid between densities instead of sliding it continuously.
fn push_grid(list: &mut DrawList, ctx: &DrawContext<'_>, screen: u32) {
    let viewport = ctx.camera.viewport();
    let zoom = ctx.camera.zoom();
    let Some(step) = grid_step(zoom) else { return };

    let visible = ctx.camera.visible_world_rect();
    let first_x = (visible.min.x / step).floor() * step;
    let first_y = (visible.min.y / step).floor() * step;
    let columns = ((visible.max.x - first_x) / step).ceil() as i64 + 1;
    let rows = ((visible.max.y - first_y) / step).ceil() as i64 + 1;
    if columns <= 0 || rows <= 0 {
        return;
    }

    let dot = GRID_DOT * ctx.camera.viewport().height.max(1.0) as f32 / 900.0;
    // Rounded, not just clamped: a dot is drawn on whole pixels below, and a whole
    // number of them is the only size that lands on them exactly.
    let dot = dot.clamp(1.0, 3.0).round().max(1.0);
    // The board's ink if it chose one, otherwise the theme's. Both branches go through the
    // same variable, so nothing below has to know which it got — a second `if` at each of
    // the three drawing sites is how a cross ends up a different colour from a dot.
    //
    // `convert` carries the document colour's alpha straight through, which is the whole
    // transparency control: `vellum-render`'s `Rgba` is *straight* alpha and its shaders
    // premultiply on output, so nothing here has to.
    let colour = ctx.grid_color.map_or(ctx.theme.grid, crate::theme::convert);
    // A grid dragged to zero opacity draws nothing, so it should not be *walked* either.
    // Note precisely what this saves and what it does not: `DrawList::push_quad` already
    // discards an invisible quad, so nothing was reaching the buffer — what was happening
    // was the `columns × rows` loop below, up to `MAX_GRID_DOTS` iterations of
    // `world_to_screen` and rounding, every frame, to produce nothing. The snap path refuses
    // on the same condition, which is what makes zero opacity mean *off* rather than merely
    // *unseen*.
    if colour.a <= 0.0 {
        return;
    }
    list.use_view(screen);

    // Lines are two runs of full-length quads rather than a dot per intersection: the
    // same spacing costs `columns + rows` quads instead of `columns × rows`, and it
    // is the only way graph paper stays inside the same budget the dots have.
    //
    // Which is also why the budget below is checked *after* this returns. It used to
    // gate both, so on a display dense enough for `columns × rows` to pass twenty
    // thousand — a 5K panel at the tight end of the spacing band — graph paper
    // vanished, charged for a cost only the dots ever pay.
    if ctx.pattern == Pattern::Lines {
        let (width, height) = (viewport.width as f32, viewport.height as f32);
        for column in 0..columns {
            let world = vellum_scene::WorldPoint::new(first_x + column as f64 * step, first_y);
            let x = ctx.camera.world_to_screen(world).x as f32;
            if x >= 0.0 && x <= width {
                list.push_quad(QuadInstance::solid([x - dot * 0.5, 0.0], [dot, height], colour));
            }
        }
        for row in 0..rows {
            let world = vellum_scene::WorldPoint::new(first_x, first_y + row as f64 * step);
            let y = ctx.camera.world_to_screen(world).y as f32;
            if y >= 0.0 && y <= height {
                list.push_quad(QuadInstance::solid([0.0, y - dot * 0.5], [width, dot], colour));
            }
        }
        return;
    }

    // Everything below is one quad per intersection, so it is the branch the budget
    // is actually about.
    if columns * rows > MAX_GRID_DOTS {
        return;
    }

    // A cross is the intersection stated with direction rather than as a speck: two
    // short bars through the point. Four times the quads of a dot field and still
    // `columns × rows`, so the same on-screen budget that bounds the dots bounds this.
    let arm = if ctx.pattern == Pattern::Crosses { (dot * 3.0).clamp(3.0, 9.0) } else { 0.0 };

    for row in 0..rows {
        for column in 0..columns {
            let world = vellum_scene::WorldPoint::new(
                first_x + column as f64 * step,
                first_y + row as f64 * step,
            );
            let at = ctx.camera.world_to_screen(world);
            if at.x < 0.0 || at.y < 0.0 || at.x > viewport.width || at.y > viewport.height {
                continue;
            }
            // Snapped to whole device pixels, size included. A mark this small spends
            // most of its area on its own edge: left unsnapped, a 1.5px dot straddles
            // two pixels, each takes partial coverage, and it composited to #CFD4D8
            // rather than the #C8CED2 it was asked for — a fifth of the contrast given
            // away to antialiasing, on the one element with least to spare. Measured
            // on a 1440×900 shot before and after. Whole pixels, whole colour.
            //
            // Those two hexes are from before the ramp was whitened; the token is
            // `#D0D6DA` now. The arithmetic is the same and so is the reason — the
            // grid kept its 27/255 distance from the canvas rather than its value.
            let (left, top) = ((at.x as f32 - dot * 0.5).round(), (at.y as f32 - dot * 0.5).round());
            if ctx.pattern == Pattern::Crosses {
                let (bar_x, bar_y) =
                    ((left + (dot - arm) * 0.5).round(), (top + (dot - arm) * 0.5).round());
                list.push_quad(QuadInstance::solid([bar_x, top], [arm, dot], colour));
                list.push_quad(QuadInstance::solid([left, bar_y], [dot, arm], colour));
            } else {
                list.push_quad(QuadInstance::solid([left, top], [dot, dot], colour));
            }
        }
    }
}

// The grid's spacing, its band, its dot size and its ceiling are all
// `vellum_project::look`'s now, shared with the browser painter — which had its own copies,
// and had taken the dot size from one round and the colour from another.
//
// `grid_step` is re-exported `pub(crate)` from the import at the top of this file **because
// Snap to grid reads it**: the spacing a gesture lands on and the spacing that is drawn have
// to be the same number, or the board says one thing and the pointer does another. That is
// the `draw::kanban_runs` rule again, and the reason `crate::snap::snap_to_grid` takes a step
// rather than working one out.

/// The minimap: the whole board in a corner, with the viewport marked on it.
///
/// Every item is drawn as a filled rectangle in its own colour rather than as a
/// generic dot, because the thing a minimap is *for* is recognising the shape of your
/// own board at a glance, and on the reference board the yellow field of stickies and
/// the dark mass of ink are what make it recognisable.
///
/// It is capped, and the cap is not a detail: the map is drawn every frame from the
/// whole document rather than from the viewport query, so it is the one place in this
/// file where cost does *not* follow the viewport. Beyond the cap it draws the
/// closest items to the camera and stops — which is exactly the part being looked at.
fn push_minimap(list: &mut DrawList, ctx: &DrawContext<'_>, screen: u32) {
    let Some([x, y, width, height]) = ctx.minimap else { return };
    let Some(content) = ctx.projection.content_bounds() else { return };
    let (content_w, content_h) = (content.max.x - content.min.x, content.max.y - content.min.y);
    if content_w <= 0.0 || content_h <= 0.0 || width <= 0.0 || height <= 0.0 {
        return;
    }

    list.use_view(screen);
    list.push_quad(
        QuadInstance::solid([x, y], [width, height], ctx.theme.surface.with_alpha(0.92))
            .with_border(ctx.theme.border, 1.0)
            .with_corner_radius(MINIMAP_RADIUS),
    );

    // Fit the content into the panel, keeping its shape, and centre it.
    let inset = MINIMAP_INSET;
    let scale = ((width - inset * 2.0) / content_w as f32)
        .min((height - inset * 2.0) / content_h as f32);
    let offset_x = x + (width - content_w as f32 * scale) * 0.5;
    let offset_y = y + (height - content_h as f32 * scale) * 0.5;
    let project = |wx: f64, wy: f64| {
        [
            offset_x + (wx - content.min.x) as f32 * scale,
            offset_y + (wy - content.min.y) as f32 * scale,
        ]
    };

    for (drawn, (_, projected)) in ctx.projection.iter().enumerate() {
        if drawn >= MAX_MINIMAP_ITEMS {
            break;
        }
        let at = project(projected.bounds.min.x, projected.bounds.min.y);
        let size = [
            ((projected.bounds.max.x - projected.bounds.min.x) as f32 * scale).max(1.0),
            ((projected.bounds.max.y - projected.bounds.min.y) as f32 * scale).max(1.0),
        ];
        let colour = crate::project::swatch(projected, ctx.theme);
        if colour.a <= 0.0 {
            continue;
        }
        list.push_quad(QuadInstance::solid(at, size, colour.with_alpha(colour.a * 0.85)));
    }

    // The viewport, as an outline. `xr-red`, the same token the selection wears,
    // because both answer "where am I".
    let visible = ctx.camera.visible_world_rect();
    let top_left = project(visible.min.x, visible.min.y);
    let bottom_right = project(visible.max.x, visible.max.y);
    list.push_quad(
        QuadInstance::solid(
            top_left,
            [
                (bottom_right[0] - top_left[0]).max(2.0),
                (bottom_right[1] - top_left[1]).max(2.0),
            ],
            Rgba::TRANSPARENT,
        )
        .with_border(ctx.theme.accent, 1.0),
    );
}

const MINIMAP_RADIUS: f32 = 6.0;
const MINIMAP_INSET: f32 = 6.0;

/// How many items the minimap will draw. Past this the map is a solid block of
/// colour anyway, and the reference board's 596 fit comfortably inside it.
const MAX_MINIMAP_ITEMS: usize = 2_000;


// `lod_band` is `vellum_project::look::lod_band`, shared with the browser painter. It is
// quantised so a pan does not re-tessellate 219 strokes every frame, and it rounds **up**:
// rounding to the nearest octave lands below the required detail for any zoom in the upper
// half of a band — at zoom 1.4 it answers 1.0, tessellating to a 0.5-world-unit tolerance
// where 0.357 was needed. That is the *"so much more pixelated"* the user reported. Rounding
// up costs vertices in the worst case and cannot cost quality.

fn tessellate_ink(points: &[vellum_doc::Point], thickness: f64, band: i32) -> vellum_ink::Mesh {
    let coordinates: Vec<(f64, f64)> = points.iter().map(|p| (p.x, p.y)).collect();
    let stroke = Stroke::from_miro(&coordinates, Some(thickness));
    stroke
        .render(Lod::new(2f64.powi(band)))
        .unwrap_or_else(|error| {
            log::warn!("ink tessellation failed: {error}");
            vellum_ink::Mesh::default()
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use vellum_doc::{Board, ItemKind, NewItem, Placement, StyledText};
    use vellum_scene::{ScreenSize, WorldRect};

    /// A title long enough to want every line `TITLE_LINES` offers.
    ///
    /// The layout tests below predate the title reservation being derived from the title's
    /// own length and are about other things — the badge, the image band, the play button —
    /// so they pass this to keep the three-line reservation they were written against. A test
    /// that is *about* the reservation passes its own count.
    const LONG_TITLE: usize = 240;

    pub(super) fn projection_with(items: impl IntoIterator<Item = NewItem>) -> Projection {
        let mut board = Board::new();
        for item in items {
            board.add(item).unwrap();
        }
        let mut projection = Projection::new();
        projection.rebuild(&board).unwrap();
        projection
    }

    /// A chosen grid ink is drawn, and it changes the colour rather than how many marks
    /// there are.
    ///
    /// **What this deliberately does not assert**: that a zero-opacity grid emits no quads.
    /// It does not, and it never did — `DrawList::push_quad` discards an invisible quad on
    /// the way in, so that assertion would pass against a build with the early return
    /// removed and would be a test of nothing. What the early return actually saves is the
    /// `columns × rows` loop, which no `DrawList` can observe. Said here rather than left
    /// implied, because an assertion that cannot fail reads exactly like one that holds.
    #[test]
    fn a_chosen_grid_ink_draws_the_same_marks_as_the_theme_s() {
        let projection = projection_with([]);
        let camera = Camera::new(ScreenSize::new(1440.0, 900.0));

        let count = |ink: Option<vellum_doc::Color>| {
            let mut ctx = context(&camera, &projection);
            ctx.pattern = Pattern::Dots;
            ctx.grid_color = ink;
            let mut list = DrawList::new();
            // A view first: `push_quad` drops everything until there is one, so a list
            // without one counts zero however well the grid works — which is exactly what
            // the first draft of this test measured.
            let screen = list.view(vellum_render::View::screen(camera.viewport()));
            push_grid(&mut list, &ctx, screen);
            list.stats().quads
        };

        let theme_grid = count(None);
        assert!(theme_grid > 0, "the default grid draws");
        assert_eq!(
            count(Some(vellum_doc::Color::rgba(0x2D, 0x6B, 0xD4, 0x80))),
            theme_grid,
            "a chosen ink changes the colour, not how many marks there are"
        );
    }

    /// A camera at `zoom`, centred on the origin, with everything a `block()` call
    /// needs around it.
    pub(super) fn context<'a>(camera: &'a Camera, projection: &'a Projection) -> DrawContext<'a> {
        DrawContext {
            agents: crate::agent_view::AgentViews::empty(),
            camera,
            projection,
            theme: Theme::LIGHT,
            selection: &[],
            hovered_badge: None,
            ports: None,
            connector_grips: None,
            default_chat_theme: vellum_agent::ChatTheme::Velm,
            default_display: DisplayMode::default(),
            marquee: None,
            placing: None,
            guides: &[],
            stroke: None,
            pending_connector: None,
            editing: None,
            card_drop: None,
            pattern: Pattern::Plain,
            grid_color: None,
            territories: Vec::new(),
            minimap: None,
        }
    }

    pub(super) fn camera_at(zoom: f64) -> Camera {
        let mut camera = Camera::new(ScreenSize::new(1600.0, 900.0));
        camera.set_zoom_about(zoom, ScreenPoint::new(800.0, 450.0));
        camera
    }

    pub(super) fn painter() -> Painter {
        Painter::new(TextCache::new().expect("the test machine has fonts"))
    }

    /// The whole point of the feature. A text item has no geometry of its own —
    /// `ItemKind::Text` pushes nothing in the board view — so before greeking existed
    /// it left a blank hole in a zoomed-out board. It must now leave a mark, and it
    /// must still draw real glyphs when there is room for them.
    #[test]
    fn text_too_small_to_read_greeks_rather_than_vanishing() {
        let projection = projection_with([NewItem::new(
            ItemKind::Text { text: StyledText::plain("Coolant System") },
            Placement::new(0.0, 0.0, 400.0, 40.0),
        )
        .with_style(Style { font_size: Some(32.0), ..Style::default() })]);
        let (&id, projected) = projection.iter().next().unwrap();
        let mut painter = painter();

        // 32 world px at 4% is 1.3 device px — unreadable, and the zoom that fits the
        // reference board.
        let camera = camera_at(0.04);
        assert!(matches!(
            painter.block(id, projected, BlockKey::PRIMARY, &context(&camera, &projection)),
            Some(Painted::Greeked(_)),
        ));

        // 32 world px at 100% is 32 device px. Real glyphs.
        let camera = camera_at(1.0);
        assert!(matches!(
            painter.block(id, projected, BlockKey::PRIMARY, &context(&camera, &projection)),
            Some(Painted::Glyphs(_)),
        ));
    }

    /// A card too small to read greeks as a **stack of title bars**, not as one grey slab.
    ///
    /// *"on Miro it's easier to see the titles immediately; on Velm I have to zoom in a lot
    /// more"*, with a screenshot of cards carrying a single mid-grey block where the wrapped
    /// title should be.
    ///
    /// The old code emitted `Single` sized `0.45 × fit.height` — the height of the **whole
    /// title-and-blurb box**. That reasoning was sound for an auto-fitted block, where the
    /// tested size and the box height are the same number, and a sticky is auto-fitted, so
    /// nothing here ever showed the fault. A card sets an explicit 13-unit size against a
    /// ~142-unit body, so the guard tested 13 and drew 64.
    ///
    /// **The assertion is on the bar height, not on the variant**, because that is the number
    /// the user was looking at: a stack of `Estimated` bars each as tall as the box would be
    /// the same slab in more pieces.
    #[test]
    fn a_card_too_small_to_read_greeks_as_lines_rather_than_one_slab() {
        let projection = projection_with([NewItem::new(
            ItemKind::LinkPreview {
                title: Some("Jtld Mufflers Performance Universal Electric Valve Muffler".into()),
                url: Some("https://www.alibaba.com/product-detail/x.html".into()),
                description: Some("Buy exhaust cutout valves at wholesale prices.".into()),
                provider: Some("Alibaba".into()),
                thumbnail: None,
                favicon: None,
                mode: CardMode::Card,
            },
            Placement::new(0.0, 0.0, 250.0, 190.0),
        )]);
        let (&id, projected) = projection.iter().next().unwrap();
        let mut painter = painter();

        // **The zoom at which a title becomes real glyphs, recorded rather than assumed.**
        //
        // It was ~38.5%, then ~29% when `TITLE_SCALE` was raised to answer *"from a distance I
        // want to be able to see more"*, and it is ~38.5% again now that the user has seen the
        // enlarged title against a Miro card and asked for it back down. That is a real
        // trade-off and it is theirs to make: the title is legible over a smaller range of
        // zooms and is not oversized at a working one. What survives from that round is the
        // *greeking*, below — the reason a small card was unreadable was never only the size.
        let camera = camera_at(0.45);
        let ctx = context(&camera, &projection);
        assert!(
            matches!(painter.block(id, projected, BlockKey::PRIMARY, &ctx), Some(Painted::Glyphs(_))),
            "the title should be readable at 45%"
        );

        // Below that, it greeks — as a stack of line bars, not as one slab. The slab was the
        // defect: the pre-shape guard tested the *font size* and then drew a bar sized from
        // the whole body box, which for a 250x190 card is ~64 units against a ~17.6-unit line.
        let camera = camera_at(0.12);
        let painted = painter.block(id, projected, BlockKey::PRIMARY, &context(&camera, &projection));
        let Some(Painted::Greeked(greek)) = painted else {
            panic!("a card at 12% must greek, not shape");
        };

        let line = card_font_size(250.0) * TITLE_SCALE * CARD_LINE_HEIGHT;
        match greek.lines {
            GreekLines::Estimated { lines, bar, .. } => {
                assert!(lines > 1, "a card body holds several lines; got {lines}");
                assert!(
                    bar <= line,
                    "a bar standing in for one line must not be taller than a line: \
                     {bar:.1} against a {line:.1}-unit line"
                );
            }
            // The number the old code produced was ~64 units against a ~17.6-unit line, so
            // this arm is what fails on it rather than a variant check that could be
            // satisfied by a slab under a different name.
            other => panic!("expected a stack of line bars, got {other:?}"),
        }
    }

    /// A caret in an **empty** sticky has to have something to be drawn against.
    ///
    /// *"when i double click on the notpad the writing status symbol which flashes this
    /// symbol | in the middle of the notepad still does not work … it only starts flashing
    /// after i start typing."* Exactly that: a wordless slot is skipped, so there was no
    /// block, no origin and nothing for `push_caret` to measure from — and the first
    /// keystroke gave the sticky words, which gave it a block, which is why the caret
    /// appeared only once typing had started.
    ///
    /// `table_cell_block` and `kanban_run_block` had each been taught this separately. The
    /// three plain kinds had not, which is the shape of the bug worth remembering: a fix
    /// applied at two of three call sites reads as done from either of them.
    #[test]
    fn an_empty_slot_holding_the_caret_still_gets_a_block() {
        let projection = projection_with([NewItem::new(
            ItemKind::Sticky { text: StyledText::default(), background: None },
            Placement::new(0.0, 0.0, 200.0, 200.0),
        )]);
        let (&id, projected) = projection.iter().next().unwrap();
        let camera = camera_at(1.0);
        let mut painter = painter();

        // With no caret in it an empty sticky is still skipped, which is what keeps a
        // board of blank notes free to shape.
        assert!(
            painter.block(id, projected, BlockKey::PRIMARY, &context(&camera, &projection)).is_none(),
            "an empty sticky nobody is typing into should cost nothing",
        );

        let mut ctx = context(&camera, &projection);
        ctx.editing = Some(TextCursor {
            scene: id,
            slot: BlockKey::PRIMARY,
            idle_for: 0.0,
            cursor: 0,
            anchor: 0,
            text: "",
        });
        assert!(
            painter.block(id, projected, BlockKey::PRIMARY, &ctx).is_some(),
            "the caret has nothing to be drawn against",
        );
    }

    /// A greeked text item now puts quads on the board where it used to put nothing
    /// at all, and it does it *without* flipping to the screen view — so the bars
    /// coalesce with the surrounding geometry instead of splitting the batch the way
    /// glyphs do.
    #[test]
    fn a_greeked_block_draws_board_quads_and_never_leaves_the_board_view() {
        let projection = projection_with([NewItem::new(
            ItemKind::Text { text: StyledText::plain("Wiring") },
            Placement::new(0.0, 0.0, 400.0, 40.0),
        )
        .with_style(Style { font_size: Some(32.0), ..Style::default() })]);
        let (&id, projected) = projection.iter().next().unwrap();
        let mut painter = painter();
        let camera = camera_at(0.04);
        let ctx = context(&camera, &projection);

        let Some(Painted::Greeked(greek)) = painter.block(id, projected, BlockKey::PRIMARY, &ctx)
        else {
            panic!("32 px text at 4% zoom is unreadable and must greek");
        };

        let mut list = DrawList::new();
        let board = list.view(View::board(&camera));
        list.use_view(board);
        painter.push_greek(&mut list, &camera, &greek, 1.0);

        assert!(list.stats().quads >= 1, "a greeked block must leave a mark");
        assert_eq!(list.stats().glyphs, 0, "greeking rasterises nothing");
        // One view, so one draw call: the bars did not split the batch.
        assert_eq!(list.stats().draw_calls, 1);
    }

    /// A bar that is sub-pixel is exactly as invisible as the glyphs it replaces, so
    /// the floor is the thing that makes this work at all.
    #[test]
    fn a_greeked_bar_never_falls_below_a_device_pixel() {
        for zoom in [1.0f64, 0.25, 0.04, 0.01] {
            let height = greek_bar_height(14.0, zoom);
            assert!(
                height * zoom >= GREEK_MIN_DEVICE_HEIGHT - 1e-9,
                "zoom {zoom} gave {height} world px, {} device px",
                height * zoom,
            );
        }
        // Where there is room, the bar is an x-height rather than the floor.
        assert!((greek_bar_height(100.0, 1.0) - 45.0).abs() < 1e-9);
    }

    /// A zoom of zero or NaN reaches here through the same camera as any other, and
    /// a NaN bar height propagates into quad geometry rather than failing loudly.
    #[test]
    fn a_nonsense_zoom_does_not_produce_nan_bars() {
        for zoom in [0.0f64, -1.0, f64::NAN, f64::INFINITY] {
            let height = greek_bar_height(14.0, zoom);
            assert!(height.is_finite() && height > 0.0, "zoom {zoom} gave {height}");
        }
    }

    /// Bars closer together than they are thick are a smear. The collapse is what
    /// bounds the quad count too: a long block never emits one bar per line at a zoom
    /// where they would overlap.
    #[test]
    fn a_stack_of_bars_collapses_to_one_when_the_lines_get_too_close() {
        let projection = projection_with([NewItem::new(
            ItemKind::Sticky {
                text: StyledText::plain("one two three four five six seven eight nine ten"),
                background: None,
            },
            Placement::new(0.0, 0.0, 200.0, 200.0),
        )]);
        let (&id, projected) = projection.iter().next().unwrap();
        let mut painter = painter();

        let mut seen_per_line = false;
        let mut seen_single = false;
        for zoom in [0.2f64, 0.12, 0.08, 0.05, 0.03, 0.02, 0.01] {
            let camera = camera_at(zoom);
            let ctx = context(&camera, &projection);
            let Some(painted) = painter.block(id, projected, BlockKey::PRIMARY, &ctx) else {
                panic!("a sticky with text always paints something at zoom {zoom}");
            };
            match painted {
                // Above the threshold the sticky is still readable; below it, greeked.
                Painted::Glyphs(_) => {}
                Painted::Greeked(greek) => match greek.lines {
                    GreekLines::PerLine { .. } => seen_per_line = true,
                    // An auto-fitted block reaches the pre-shape guard with a box that is one
                    // line tall by construction — `largest_font_size` *is* `fit.height` — so
                    // a sticky must never estimate a stack. A card does; that is the split
                    // `a_card_too_small_to_read_greeks_as_lines_rather_than_one_slab` covers.
                    GreekLines::Estimated { lines, .. } => {
                        panic!("an auto-fitted sticky estimated {lines} lines at zoom {zoom}")
                    }
                    GreekLines::Single { width, height } => {
                        seen_single = true;
                        assert!(width > 0.0 && height > 0.0, "zoom {zoom}");
                    }
                },
            }
        }
        assert!(seen_per_line, "a wrapped sticky greeks per line while the lines are apart");
        assert!(seen_single, "and collapses to one bar once they are not");
    }

    /// The pre-shaping bail is the hot path — it exists so that fitting the reference
    /// board does not auto-fit 236 blocks — and it must keep bailing. Greeking it
    /// cannot be allowed to start shaping what it was built to avoid.
    #[test]
    fn greeking_a_tiny_block_still_shapes_nothing() {
        let projection = projection_with([NewItem::new(
            ItemKind::Sticky { text: StyledText::plain("sample note"), background: None },
            Placement::new(0.0, 0.0, 200.0, 200.0),
        )]);
        let (&id, projected) = projection.iter().next().unwrap();
        let mut painter = painter();
        // 200 px tall at 1% is 2 device px: under the bound `largest_font_size` gives,
        // so this returns before the auto-fit binary search.
        let camera = camera_at(0.01);
        let ctx = context(&camera, &projection);

        assert!(matches!(
            painter.block(id, projected, BlockKey::PRIMARY, &ctx),
            Some(Painted::Greeked(Greek { lines: GreekLines::Single { .. }, .. })),
        ));
        assert_eq!(painter.text.len(), 0, "the pre-shaping bail must not shape");
    }

    /// Greeking stands in for text that exists. An empty slot has none, and must stay
    /// empty rather than growing a bar out of nothing.
    #[test]
    fn an_empty_text_slot_greeks_nothing() {
        let projection = projection_with([
            NewItem::new(
                ItemKind::Sticky { text: StyledText::default(), background: None },
                Placement::new(0.0, 0.0, 200.0, 200.0),
            ),
            NewItem::new(
                ItemKind::Image { asset_id: "x".into(), crop: None },
                Placement::new(600.0, 0.0, 200.0, 200.0),
            ),
        ]);
        let mut painter = painter();
        let camera = camera_at(0.04);
        let ctx = context(&camera, &projection);

        for (&id, projected) in projection.iter() {
            for slot in 0..painter.slots_of(id, projected, projected.generation, &ctx) {
                assert!(
                    painter.block(id, projected, slot, &ctx).is_none(),
                    "no text means no block, and no bar",
                );
            }
        }
    }

    /// The bars have to land where the glyphs would have, or a board pops sideways as
    /// it crosses the threshold. Both paths run through `locate_world`, so this pins
    /// that they agree.
    #[test]
    fn a_bar_sits_where_the_text_it_replaces_would_have_sat() {
        let projection = projection_with([NewItem::new(
            ItemKind::Text { text: StyledText::plain("ECU") },
            Placement::new(120.0, -80.0, 400.0, 40.0),
        )
        .with_style(Style { font_size: Some(32.0), ..Style::default() })]);
        let (&id, projected) = projection.iter().next().unwrap();
        let mut painter = painter();

        let readable = camera_at(1.0);
        let Some(Painted::Glyphs(block)) =
            painter.block(id, projected, BlockKey::PRIMARY, &context(&readable, &projection))
        else {
            panic!("32 px text at 100% is readable");
        };
        let glyph_origin = readable.screen_to_world(block.origin);

        let tiny = camera_at(0.04);
        let Some(Painted::Greeked(greek)) =
            painter.block(id, projected, BlockKey::PRIMARY, &context(&tiny, &projection))
        else {
            panic!("32 px text at 4% is not");
        };

        // Left edges coincide; the tops differ only by the shaped-versus-bounding
        // height the two paths have available, which is under a device pixel here.
        assert!((greek.origin.x - glyph_origin.x).abs() < 1.0, "{greek:?} vs {glyph_origin:?}");
    }

    #[test]
    fn the_lod_band_is_a_whole_octave_and_survives_nonsense() {
        assert_eq!(lod_band(1.0), 0, "an exact octave is its own band");
        assert_eq!(lod_band(2.0), 1);
        assert_eq!(lod_band(0.25), -2);
        assert_eq!(lod_band(0.0), 0);
        assert_eq!(lod_band(f64::NAN), 0);
        assert!((-8..=8).contains(&lod_band(1e30)));
    }

    /// The band must **round up**, never down.
    ///
    /// It used to round to the nearest octave, which for any zoom in the upper half of a
    /// band answers a band *below* the one needed — at 1.3 it said 1.0, so the stroke was
    /// tessellated to a 0.5-world-unit tolerance where 0.385 was required. Rounding up costs
    /// vertices in the worst case; rounding down costs visible faceting on every cap and
    /// join, which is what the user saw as *"so much more pixelated"*.
    ///
    /// This is the assertion that fails on the old `round()`, and it is deliberately phrased
    /// as the *property* rather than as a table of answers, because a table is what let the
    /// old behaviour look intentional: `lod_band(1.3) == 0` was written down as expected.
    #[test]
    fn the_lod_band_never_under_tessellates() {
        for zoom in [0.03f64, 0.3, 0.7, 1.0, 1.3, 1.41, 1.99, 2.0, 5.0, 64.0] {
            let band = lod_band(zoom);
            let tolerance_for = 2f64.powi(band);
            assert!(
                tolerance_for >= zoom - 1e-9,
                "at zoom {zoom} the band answers {tolerance_for}, which is coarser than the \
                 zoom it is drawn at"
            );
        }
    }

    /// A band change is what re-tessellates; a pan must not. If this stops holding,
    /// panning a board of ink re-runs 219 stroke pipelines every frame.
    #[test]
    fn zooming_within_a_band_does_not_change_the_band() {
        // Both inside the (1, 2] octave, which `ceil` maps to band 1.
        assert_eq!(lod_band(1.1), lod_band(1.9));
        assert_ne!(lod_band(1.9), lod_band(2.1));
    }

    #[test]
    fn ink_tessellates_into_finite_triangles() {
        let points = vec![
            vellum_doc::Point::new(-50.0, -20.0),
            vellum_doc::Point::new(0.0, 30.0),
            vellum_doc::Point::new(50.0, -20.0),
        ];
        let mesh = tessellate_ink(&points, 6.0, 0);
        assert!(mesh.triangle_count() > 0);
        assert!(mesh.is_finite());
    }

    /// A stroke with no points, or one point, is a shape a Miro import really
    /// produces — a tap of the pen. Neither may panic, and a single point still has
    /// to draw the dot the user made.
    #[test]
    fn a_degenerate_stroke_produces_finite_geometry_rather_than_a_panic() {
        assert!(tessellate_ink(&[], 4.0, 0).is_empty());

        let dot = tessellate_ink(&[vellum_doc::Point::new(0.0, 0.0)], 4.0, 0);
        assert!(dot.is_finite());
        for position in &dot.positions {
            assert!(position[0].abs() <= 2.5 && position[1].abs() <= 2.5, "{position:?}");
        }
    }



    /// The three blocks hold three different things, and none of them holds another's.
    ///
    /// The card was two blocks and is now three — a muted provider row, a large dark title
    /// and a smaller grey blurb — because a block carries one colour *and* one size, so
    /// Miro's arrangement is not expressible in fewer. The separation is what this asserts:
    /// a title that also appears in the blurb block is the card drawing its own name twice,
    /// which is the fault the user photographed arriving by a different route.
    #[test]
    fn a_cards_three_blocks_each_hold_their_own_part() {
        let kind = ItemKind::link_preview(
            Some("Compact cooling fan".into()),
            Some("https://example.com/cooling".into()),
            None,
        );
        let title = card_text(&kind, BlockKey::PRIMARY, usize::MAX).to_plain();
        let blurb = card_text(&kind, CARD_BLURB_SLOT, usize::MAX).to_plain();

        assert!(title.contains("Compact cooling fan"), "the title block holds the title: {title}");
        assert!(!title.contains("example.com"), "the address is not the title's: {title}");
        // With no blurb to show, the link is worth the room after all — in the blurb's block.
        assert!(blurb.contains("https://example.com/cooling"), "{blurb}");
        // Empty, because this card carries no `provider` — the host-derived name is filled in
        // by `vellum_import::pipeline::link_kind` and by the fetch pool, not here. Worth
        // asserting rather than skipping: it is the row that must *not* pick up the title.
        assert!(card_text(&kind, BlockKey::SECONDARY, usize::MAX).is_empty());
    }

    /// A frame's name survives being zoomed out of — which is the entire job of a frame's name.
    ///
    /// *"also please dont make the titles of the frames small like this thank you"*, with the
    /// same board photographed in both applications: Miro's "Education" and "Food" large and
    /// legible over small frames, Velm's an illegible smear. The smear was literal — a frame
    /// title greeks as `GreekLines::Single`, which is **one grey bar as wide as the whole
    /// frame**, and at the default frame size that began below 18.5% zoom.
    ///
    /// The A/B is the low-zoom half: with the `draw_scale` floor removed this greeks. The
    /// high-zoom half is not decoration — a clamp that stayed engaged would pin every frame
    /// title to 13 px and stop it growing with its frame, which is the opposite complaint.
    #[test]
    fn a_frame_title_never_greeks_however_far_the_board_is_zoomed_out() {
        let projection = projection_with([NewItem::new(
            ItemKind::Frame {
                title: StyledText::plain("Education"),
                order: None,
                speaker_notes: None,
            },
            // The default a frame is placed at, so the numbers here are the ones a user meets.
            Placement::new(0.0, 0.0, 1600.0, 900.0),
        )]);
        let (&id, projected) = projection.iter().next().unwrap();
        let mut painter = painter();

        // 900 x 0.03 = 27 world units, so before the floor this was 27 x zoom device pixels:
        // 2.7 at 10%, well under `MIN_DEVICE_FONT_SIZE`, and greeked.
        for zoom in [0.04, 0.10, 0.18, 0.5] {
            let camera = camera_at(zoom);
            let painted =
                painter.block(id, projected, BlockKey::SECONDARY, &context(&camera, &projection));
            assert!(
                matches!(painted, Some(Painted::Glyphs(_))),
                "a frame title must be real glyphs at {}% zoom, not a bar: {painted:?}",
                zoom * 100.0
            );
            let Some(Painted::Glyphs(block)) = painted else { unreachable!() };
            let drawn = f64::from(block.font_size) * f64::from(block.scale);
            assert!(
                drawn >= FRAME_TITLE_MIN_DEVICE - 0.01,
                "…and at least {FRAME_TITLE_MIN_DEVICE} device pixels: {drawn:.2} at {}%",
                zoom * 100.0
            );
        }

        // Zoomed *in*, the floor is inert and the world size takes over again — a frame's name
        // still grows with its frame, which is what `FRAME_TITLE_FRACTION` is for.
        let camera = camera_at(2.0);
        let painted =
            painter.block(id, projected, BlockKey::SECONDARY, &context(&camera, &projection));
        let Some(Painted::Glyphs(block)) = painted else { panic!("glyphs at 200%") };
        assert!(
            (f64::from(block.scale) - 2.0).abs() < 1e-6,
            "the clamp must not still be engaged at 200%: scale {}",
            block.scale
        );
    }

    /// The three blocks read as three voices, and the title hangs off the site name rather
    /// than floating in the middle of the card.
    ///
    /// *"i am tryint ofigure out why my velm looks little bit off"*, with a Miro card open
    /// beside a Velm one. Four faults in one picture, all measured off that pair:
    ///
    /// - the site name was the **base size** — identical to the title and *larger* than the
    ///   blurb, so the card's biggest text was the thing on it that matters least;
    /// - **nothing was bold**, though two doc comments in this file had claimed "a large bold
    ///   title" since the third block was added;
    /// - the blurb took every line left over, six or more on a tall card;
    /// - and `TITLE_LINES` was reserved flat, so a one-line title left **two empty lines**
    ///   before the blurb and a three-line title left none.
    ///
    /// The last is the one that reads as a bug rather than as a style: air a reader cannot
    /// account for always does.
    #[test]
    fn a_cards_three_voices_are_three_sizes_and_the_blurb_hangs_off_the_title() {
        let (w, h, font) = (250.0, 250.0, card_font_size(250.0));

        // Sizes, in the order `Painter::block` resolves them. Strictly ordered, and the
        // *provider* is the one that moved — the title deliberately did not, because it was
        // raised once and the user asked for it back down.
        let (name, title, blurb) =
            (font * PROVIDER_SCALE, font * TITLE_SCALE, font * BLURB_SCALE);
        assert!(
            name < blurb && blurb < title,
            "name {name:.2} < blurb {blurb:.2} < title {title:.2}"
        );

        // A one-line title. The blurb must start within a line and the gap of it — not two
        // lines below, which is what the flat reservation gave and what was photographed.
        let short = "Crush 80 Reboot Pro";
        let laid = card_layout(w, h, font, CardMode::Card, true, true, true, false, short.len());
        let (_, ty, _, th) = laid.title;
        let (_, by, _, bh) = laid.blurb;
        let line = font * TITLE_SCALE * CARD_LINE_HEIGHT;
        assert!(
            (th - line).abs() < 0.01,
            "a one-line title reserves one line: {th:.2} against {line:.2}"
        );
        // The A/B: on the flat `TITLE_LINES` reservation this was `ty + 3 * line`.
        assert!(
            by - ty < line * 2.0,
            "the blurb hangs off the title, not two empty lines below it: \
             title at {ty:.1}, blurb at {by:.1}, line {line:.2}"
        );

        // …and the blurb is capped rather than taking the rest of the card. *"less
        // decription more white space"*.
        let blurb_line = font * BLURB_SCALE * CARD_LINE_HEIGHT;
        assert!(
            bh <= blurb_line * BLURB_LINES + 0.01,
            "the blurb stops at {BLURB_LINES} lines: {bh:.2} against {:.2}",
            blurb_line * BLURB_LINES
        );

        // A long title still gets every line it is allowed, so shortening the reservation did
        // not turn into truncating the thing the card is named after.
        let long = "a".repeat(400);
        let tall = card_layout(w, h, font, CardMode::Card, true, true, true, false, long.len());
        assert!(
            (tall.title.3 - line * TITLE_LINES).abs() < 0.01,
            "a long title still gets {TITLE_LINES} lines: {:.2}",
            tall.title.3
        );

        // And the title is drawn **bold**. Asserted on the span rather than on a measured
        // width, because a width assertion cannot tell "heavier" from "different" — the trap
        // that let every bold span on the board render in Courier for a whole round.
        let kind = ItemKind::link_preview(
            Some(short.into()),
            Some("https://example.com/k".into()),
            None,
        );
        let spans = card_text(&kind, BlockKey::PRIMARY, usize::MAX);
        assert!(
            spans.spans().iter().any(|s| s.style.bold),
            "the title block carries a bold span"
        );
    }

    /// What each mode says, and what it leaves out. The three are different amounts of card,
    /// so this is the assertion that keeps them from collapsing into one appearance.
    #[test]
    fn each_card_mode_shows_a_different_amount() {
        let card = |mode| ItemKind::LinkPreview {
            title: Some("Widget Pro 2.0".into()),
            url: Some("https://www.aliexpress.us/item/1.html".into()),
            description: Some("A low-profile switch.".into()),
            thumbnail: None,
            provider: Some("AliExpress".into()),
            favicon: None,
            mode,
        };

        // Collapsed: the site and the title, on one line, and no blurb.
        let link = card_text(&card(CardMode::Link), BlockKey::SECONDARY, usize::MAX).to_plain();
        assert!(link.contains("AliExpress") && link.contains("Widget Pro 2.0"), "{link}");
        assert!(!link.contains("low-profile"), "a collapsed row carries no blurb: {link}");
        assert!(!link.contains('\n'), "and it is one line: {link:?}");
        for slot in [BlockKey::PRIMARY, CARD_BLURB_SLOT] {
            assert!(
                card_text(&card(CardMode::Link), slot, usize::MAX).is_empty(),
                "a collapsed row is its provider row and nothing else"
            );
        }

        // Card and Large put the title and the blurb in *separate* blocks, so they can be set
        // at different sizes, and neither repeats the URL when there is a blurb to show.
        for mode in [CardMode::Card, CardMode::Large] {
            let title = card_text(&card(mode), BlockKey::PRIMARY, usize::MAX).to_plain();
            let blurb = card_text(&card(mode), CARD_BLURB_SLOT, usize::MAX).to_plain();
            assert!(title.contains("Widget Pro 2.0"), "{mode:?} title: {title}");
            assert!(blurb.contains("A low-profile switch."), "{mode:?} blurb: {blurb}");
            assert!(
                !blurb.contains("Widget Pro 2.0"),
                "{mode:?} drew the title again under itself: {blurb}"
            );
            assert!(
                !blurb.contains("aliexpress.us/item"),
                "{mode:?} repeated the URL over the blurb: {blurb}"
            );
        }
    }

    #[test]
    fn an_embed_names_its_provider_and_a_document_its_page_count() {
        let embed = ItemKind::embed(
            Some("Assembly".into()),
            None,
            None,
            Some("YouTube".into()),
            None,
        );
        // The provider is the `SECONDARY` row, in its own muted colour — not part of the
        // title's block. That two-tone split is most of what makes Miro's card read as a card.
        assert!(card_text(&embed, BlockKey::SECONDARY, usize::MAX).to_plain().contains("YouTube"));
        let primary = card_text(&embed, BlockKey::PRIMARY, usize::MAX).to_plain();
        assert!(primary.contains("Assembly"), "{primary}");
        assert!(!primary.contains("YouTube"), "the site is not repeated in the title: {primary}");

        let one = ItemKind::Document { asset_id: "h".into(), page_count: 1, current_page: 0 };
        assert!(card_text(&one, BlockKey::PRIMARY, usize::MAX).to_plain().contains("1 page"));
        let many = ItemKind::Document { asset_id: "h".into(), page_count: 12, current_page: 0 };
        assert!(card_text(&many, BlockKey::PRIMARY, usize::MAX).to_plain().contains("12 pages"));
    }

    /// The collapsed row is one line, and a long title is cut rather than wrapped.
    #[test]
    fn the_collapsed_row_clips_to_its_budget() {
        let card = ItemKind::LinkPreview {
            title: Some("Widget Pro 2.0 Mechanical Keyboard".into()),
            url: Some("https://www.aliexpress.us/item/1.html".into()),
            description: None,
            thumbnail: None,
            provider: Some("AliExpress".into()),
            favicon: None,
            mode: CardMode::Link,
        };
        let clipped = card_text(&card, BlockKey::SECONDARY, 30).to_plain();
        assert!(clipped.chars().count() <= 30, "{clipped:?} is {} chars", clipped.chars().count());
        assert!(clipped.starts_with("AliExpress"), "{clipped:?}");
        assert!(clipped.ends_with('…'), "a cut row says so: {clipped:?}");
        assert!(!clipped.contains('\n'), "still one line: {clipped:?}");

        // Room to spare means no ellipsis at all.
        let whole = card_text(&card, BlockKey::SECONDARY, 200).to_plain();
        assert!(whole.ends_with("Keyboard"), "{whole:?}");
    }

    /// The card's internal geometry, which four separate draw paths have to agree on.
    ///
    /// Asserted as *relationships* rather than as numbers — the image above the site name, the
    /// name above the title, everything inside the card — so a change to the padding or the
    /// image fraction does not have to be mirrored here to keep the test true.
    #[test]
    fn a_cards_pieces_stack_in_miros_order_and_stay_inside_it() {
        let (w, h, font) = (250.0, 190.0, 13.0);

        // Large, with a picture: image, then favicon and site name, then the title block.
        let large = card_layout(w, h, font, CardMode::Large, true, true, true, false, LONG_TITLE);
        let (ix, iy, iw, ih) = large.image.expect("a large card with an image draws one");
        let (fx, fy, fw, fh) = large.favicon.expect("and its site's icon");
        let (px, py, ..) = large.provider;
        let (bx, by, bw, bh) = large.body;

        assert!(ix > 0.0 && iy > 0.0, "the image is inset, not bled to the edge: {:?}", large.image);
        assert!(ix + iw < w && iy + ih < h, "and stays inside the card");
        assert!(fy >= iy + ih, "the icon sits below the image, not on it");
        assert!(px > fx + fw * 0.9, "the site name starts right of its icon");
        assert!((py - fy).abs() < fh, "and on the same row");
        assert!(by > py, "the title is below the site name");
        assert!(bx + bw <= w && by + bh <= h, "the body stays inside the card");

        // No image fetched: nothing reserves the band, so the text starts at the top.
        let unfetched = card_layout(w, h, font, CardMode::Large, false, true, true, false, LONG_TITLE);
        assert!(unfetched.image.is_none(), "no picture, no band");
        assert!(
            unfetched.provider.1 < py,
            "the site name moves up to where the picture would have been"
        );

        // A plain `Card` shows its picture too — that is the mode a pasted link is in, so
        // restricting the image to `Large` meant a fetched YouTube poster frame sat on disk
        // while the card drew a box of text. The two differ in how much of the card it takes.
        let ordinary = card_layout(w, h, font, CardMode::Card, true, true, true, false, LONG_TITLE);
        let (_, _, _, card_image_h) = ordinary.image.expect("a Card with an image draws it");
        assert!(card_image_h < ih, "and gives it less room than Large: {card_image_h} vs {ih}");
        assert!(ordinary.body.3 > large.body.3, "leaving more room for the blurb");

        // Collapsed: one line, an icon, and no body at all.
        let row = card_layout(w, h, font, CardMode::Link, true, true, true, false, LONG_TITLE);
        assert!(row.image.is_none(), "a collapsed row draws no picture even when one exists");
        assert!(row.favicon.is_some(), "but it keeps its icon — that is what makes rows scannable");
        assert_eq!(row.body.3, 0.0, "and has no second block");
        assert!(row.provider.3 <= font * CARD_LINE_HEIGHT + 0.01, "one line tall");

        // Without an icon the row is not indented for one.
        let bare = card_layout(w, h, font, CardMode::Card, false, false, true, false, LONG_TITLE);
        assert!(bare.favicon.is_none());
        assert!(bare.provider.0 < px, "no icon, no indent: {} vs {px}", bare.provider.0);
    }

    /// *"on the top right corner of each widget have an button that will take me to the
    /// website"* — where that badge is, in all three card forms.
    ///
    /// The corner it is in is the whole feature, so it is asserted as a corner rather than as
    /// a pair of numbers: right of centre, above centre, and inside the card.
    #[test]
    fn every_card_form_carries_an_open_badge_in_its_top_right_corner() {
        let (w, h, font) = (250.0, 190.0, 13.0);
        for mode in CardMode::ALL {
            let laid = card_layout(w, h, font, mode, true, true, true, false, LONG_TITLE);
            let (bx, by, bw, bh) = laid.badge.unwrap_or_else(|| panic!("{mode:?} has no badge"));

            assert!(bx > w / 2.0, "{mode:?}: not in the right half");
            assert!(by < h / 2.0, "{mode:?}: not in the top half");
            assert!(bx + bw <= w && by + bh <= h, "{mode:?}: hangs outside the card");
            assert!(bx > 0.0 && by > 0.0, "{mode:?}: flush against the edge, not inset");
            assert!((bw - bh).abs() < 1e-9, "{mode:?}: a badge is square");
            assert!(bw > font, "{mode:?}: smaller than the type it sits beside");

            // Nothing to open, no button. A badge that answers "that card's address is not a
            // web page" is worse than no badge.
            assert!(
                card_layout(w, h, font, mode, true, true, false, false, LONG_TITLE).badge.is_none(),
                "{mode:?}: a card with no address still offered one"
            );
        }
    }

    /// The badge and the site name must not end up on top of each other — and the fix for
    /// that must not cost every card characters it did not need to lose.
    ///
    /// Both halves matter. Clipping the provider row unconditionally is the easy version and
    /// is wrong: on a card with a picture the badge is up on the image and the site name is
    /// far below it, so an unconditional margin just makes titles mysteriously short.
    #[test]
    fn the_site_name_yields_to_the_badge_only_when_they_share_a_row() {
        let (w, h, font) = (250.0, 190.0, 13.0);

        // Sharing: a collapsed row *is* one row, and a card whose picture has not arrived
        // has its provider at the top.
        for (name, laid) in [
            ("collapsed", card_layout(w, h, font, CardMode::Link, true, true, true, false, LONG_TITLE)),
            ("unfetched", card_layout(w, h, font, CardMode::Card, false, true, true, false, LONG_TITLE)),
        ] {
            let (px, _, pw, _) = laid.provider;
            let (bx, ..) = laid.badge.expect("a badge");
            assert!(px + pw <= bx, "{name}: the site name runs under the badge");
            let without = match name {
                "collapsed" => card_layout(w, h, font, CardMode::Link, true, true, false, false, LONG_TITLE),
                _ => card_layout(w, h, font, CardMode::Card, false, true, false, false, LONG_TITLE),
            };
            assert!(pw < without.provider.2, "{name}: the row did not actually yield");
        }

        // Not sharing: a picture pushes the site name below the badge entirely, so the row
        // keeps its full width.
        let with_picture = card_layout(w, h, font, CardMode::Large, true, true, true, false, LONG_TITLE);
        let no_badge = card_layout(w, h, font, CardMode::Large, true, true, false, false, LONG_TITLE);
        assert!(
            (with_picture.provider.2 - no_badge.provider.2).abs() < 1e-9,
            "the badge shortened a row it does not touch"
        );
        let (_, by, _, bh) = with_picture.badge.expect("a badge");
        assert!(with_picture.provider.1 >= by + bh, "…but they really are on different rows");
    }

    /// Three ways the badge's geometry was wrong, all found by review rather than by use.
    ///
    /// Each is a case the first version's own tests walked straight past, because they all
    /// used one comfortable card shape and asked only about the badge itself.
    #[test]
    fn the_badge_stays_inside_awkward_cards_and_off_the_words() {
        let font = 13.0;

        // 1. A wide, short card. `pad` is derived from the **width**, so a card 600 wide and
        //    26 tall put the badge's top at 36 — ten points below its own bottom edge.
        for (w, h) in [(600.0, 26.0), (600.0, 8.0), (120.0, 400.0), (40.0, 40.0)] {
            for mode in CardMode::ALL {
                let laid = card_layout(w, h, font, mode, true, true, true, false, LONG_TITLE);
                if let Some((bx, by, bw, bh)) = laid.badge {
                    assert!(
                        bx >= 0.0 && by >= 0.0 && bx + bw <= w + 1e-9 && by + bh <= h + 1e-9,
                        "{mode:?} on a {w}x{h} card put the badge at {:?}",
                        laid.badge
                    );
                }
            }
        }

        // 2. The body yields to the badge. Without a picture the provider is at the top and
        //    the badge — 1.75 line-heights tall against a one-line row — reaches past it into
        //    the *title*, which is the line most worth reading.
        let bare = card_layout(250.0, 190.0, font, CardMode::Card, false, true, true, false, LONG_TITLE);
        let (bx, ..) = bare.badge.expect("a badge");
        let (px, _, pw, _) = bare.provider;
        let (bodyx, _, bodyw, _) = bare.body;
        assert!(px + pw <= bx, "the site name runs under the badge");
        assert!(bodyx + bodyw <= bx, "the title runs under the badge");
        // …and with a picture, neither is shortened: the badge is up on the image.
        let with_picture = card_layout(250.0, 190.0, font, CardMode::Large, true, true, true, false, LONG_TITLE);
        let no_badge = card_layout(250.0, 190.0, font, CardMode::Large, true, true, false, false, LONG_TITLE);
        assert_eq!(with_picture.body.2, no_badge.body.2, "the title lost width for nothing");

        // 3. A card too short to hold one gets none at all, rather than a sliver.
        assert!(card_layout(250.0, 1.0, font, CardMode::Card, false, false, true, false, LONG_TITLE).badge.is_none());
    }

    /// *"the images are all distorted"* — and what the fix has to guarantee.
    ///
    /// The assertion is about the **shape the pixels end up**, not about the numbers: whatever
    /// crop is chosen, the sampled region's aspect has to equal the box's, because that is the
    /// definition of "not stretched". Asserting the UV values themselves would pass just as
    /// happily on a crop that was wrong in a different way.
    #[test]
    fn a_cards_picture_is_cropped_to_its_band_rather_than_stretched_into_it() {
        // A card's real image band, from the layout above.
        let laid = card_layout(250.0, 190.0, 13.0, CardMode::Card, true, true, true, false, LONG_TITLE);
        let (_, _, bw, bh) = laid.image.expect("a Card with an image");
        let box_aspect = bw / bh;

        // The three shapes the board actually holds: a wide `og:image` banner, a square
        // product shot, and a tall poster. The square one is the case that was worst — it was
        // being stretched by the full difference between 1.00 and the band's aspect.
        for (name, source) in
            [("banner", (1200, 630)), ("product", (1000, 1000)), ("poster", (600, 900))]
        {
            let uv = cover_uv(source, (bw, bh));
            let (u, v) = (f64::from(uv.max[0] - uv.min[0]), f64::from(uv.max[1] - uv.min[1]));
            assert!(u > 0.0 && v > 0.0, "{name}: sampled nothing");
            assert!(
                uv.min[0] >= 0.0 && uv.min[1] >= 0.0 && uv.max[0] <= 1.0 && uv.max[1] <= 1.0,
                "{name}: sampled outside the texture: {uv:?}"
            );

            // The sampled region, in texels, has the band's aspect. This is the whole claim.
            let sampled = (u * f64::from(source.0)) / (v * f64::from(source.1));
            assert!(
                (sampled - box_aspect).abs() < 1e-6,
                "{name}: sampled {sampled:.4} into a {box_aspect:.4} band — still distorted"
            );

            // Exactly one axis is trimmed, and it is trimmed equally at both ends so a
            // centred subject stays centred. A product photo is centred on white by
            // convention, which is what makes cover safe here rather than merely conventional.
            assert!(u == 1.0 || v == 1.0, "{name}: both axes cropped, so it was scaled twice");
            assert!(
                (uv.min[0] - (1.0 - uv.max[0])).abs() < 1e-6
                    && (uv.min[1] - (1.0 - uv.max[1])).abs() < 1e-6,
                "{name}: the crop is off-centre: {uv:?}"
            );
        }

        // The control. Without the fix every one of those was `FULL`, and `FULL` only has the
        // band's aspect when the source already does — so this is the case that used to pass
        // by accident, and it is the reason the assertions above measure the *sampled* aspect
        // rather than the UV numbers.
        //
        // Approximately, not exactly: a texel count is an integer and the band's is not, so
        // the nearest whole-texel source is a hair off the band and is correctly trimmed by a
        // fraction of a texel. Demanding `== FULL` here would be demanding that the function
        // round in the caller's favour.
        // **Derived from the band, not hardcoded.** It used to be a literal `(2200, 724)`,
        // chosen because it matched the band's aspect at the padding of the day — so changing
        // `CARD_PADDING` to Miro's proportions made a control that is *supposed* to need no
        // crop get trimmed by 1.4%, and the test failed for a reason that had nothing to do
        // with cropping. A control computed from the thing it is a control for cannot drift.
        #[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss, reason = "a texel count")]
        let matching = cover_uv((((724.0 * box_aspect).round()) as u32, 724), (bw, bh));
        assert!(
            matching.min[0] < 0.005 && matching.max[0] > 0.995,
            "a source already the right shape was cropped: {matching:?}"
        );

        // Degenerate inputs answer `FULL` rather than dividing by zero: a zero-sized item is
        // one resize-handle drag away.
        assert_eq!(cover_uv((0, 0), (bw, bh)), vellum_render::UvRect::FULL);
        assert_eq!(cover_uv((100, 100), (0.0, 0.0)), vellum_render::UvRect::FULL);
    }

    /// The two shapes of duplicate that Miro's own metadata actually contains.
    ///
    /// Both taken from `captures/reference-board.html`, where **18 of 91 preview widgets**
    /// carry a description that says what the title already said. The truncated one is the
    /// case a plain `==` misses, and it is the more common of the two.
    #[test]
    fn a_blurb_that_repeats_the_title_is_not_drawn_twice() {
        let title = "Competition Intercooler For M5 M6 F10 F12 F13 Intercooler - Buy M5 F10 \
                     Intercooler for M6 Custom Intercooler Product on Alibaba.com";
        assert!(says_the_same_as(Some(title), title), "byte-identical");

        let truncated = "Competition Intercooler For M5 M6 F10 F12 F13 Intercooler - Buy M5 \
                         F10 Intercooler for M6 Custom Interc…";
        assert!(says_the_same_as(Some(title), truncated), "a truncated prefix of the title");

        // Punctuation differs between the two fields on the real board — an em dash against a
        // hyphen, a trailing full stop, `&` against `and` — so the comparison cannot be on
        // raw bytes.
        assert!(
            says_the_same_as(
                Some("Exhaust Rubber Hanger — Buy Rubber Hangers on Alibaba.com"),
                "Exhaust Rubber Hanger - Buy Rubber Hangers on Alibaba.com."
            ),
            "punctuation must not defeat it"
        );

        // **Entities are decoded upstream, not here**, and this pins the division so nobody
        // adds a second decoder to this function. The reference board stores `&#43;` in the
        // title where the description has `+`, and `&#43;` normalises to the *digits* `43` —
        // so undecoded, these two genuinely are different strings and this correctly says so.
        // `vellum_import::pipeline::link_kind` runs `decode_entities` over both fields, which
        // is what makes them meet; a fetched card never carries entities at all, because
        // `vellum-link` parses real HTML.
        assert!(
            !says_the_same_as(
                Some("For 2018&#43; Acme M5 M8 G90 F90 Product"),
                "For 2018+ Acme M5 M8 G90 F90 Product"
            ),
            "raw entities are the importer's job; this must not grow a second decoder"
        );
    }

    /// A real blurb survives — which is the half that would be silently lost.
    ///
    /// A suppressed duplicate is invisible when it is wrong: the card simply has no
    /// description and looks fine. So these are the assertions that matter, and both name a
    /// guard that a simpler `starts_with` would fail.
    #[test]
    fn a_real_blurb_is_kept_even_when_it_opens_with_the_title() {
        assert!(
            !says_the_same_as(
                Some("Alibaba.com"),
                "Alibaba.com is the world's largest marketplace for wholesale goods, \
                 connecting buyers with millions of suppliers."
            ),
            "a short site-name title must not swallow the page's actual description"
        );
        assert!(
            !says_the_same_as(Some("Acme M5 Competition Intercooler"), "Fits F90 chassis only."),
            "a genuinely different blurb is kept"
        );
        assert!(!says_the_same_as(None, "Any description at all"), "no title, nothing to repeat");
        assert!(
            !says_the_same_as(Some("Exhaust"), "Exhaust"),
            "too short to tell a duplicate from a one-word blurb"
        );
    }

    /// A video card gets a ▶ on its poster; nothing else does.
    ///
    /// *"the YouTube previews on Miro I like more because I can just open it and view the
    /// video right then and there."* It opens the browser rather than playing inline — that
    /// needs an engine `docs/01-architecture.md` §1 rules out — but the mark is what makes a
    /// video card recognisable as one from across the board.
    ///
    /// **The negatives are the assertions that matter.** Nearly every card has a poster and
    /// almost none are videos, so a predicate that answers "has a picture" would put a play
    /// button on every product photo on this board — promising playback that pressing it
    /// cannot deliver, which is the dead-button failure `has_link` already exists to prevent
    /// one class of.
    #[test]
    fn only_a_video_card_gets_a_play_button() {
        let laid = |video, image| {
            card_layout(250.0, 190.0, 13.0, CardMode::Large, image, true, true, video, LONG_TITLE)
        };
        let (px, py, pw, ph) = laid(true, true).play.expect("a YouTube card plays");
        let (ix, iy, iw, ih) = laid(true, true).image.expect("a Large card with a picture");
        assert!(pw > 0.0 && (pw - ph).abs() < f64::EPSILON, "square: {pw} x {ph}");
        // Centred on the **poster**, not on the card — a ▶ in the text block aims at nothing.
        assert!(
            (px + pw / 2.0 - (ix + iw / 2.0)).abs() < 0.5
                && (py + ph / 2.0 - (iy + ih / 2.0)).abs() < 0.5,
            "the play button is not centred on the image band"
        );

        assert!(laid(false, true).play.is_none(), "an ordinary card with a photo gets no ▶");
        assert!(laid(true, false).play.is_none(), "no poster is nothing to centre it on");
        assert!(
            card_layout(250.0, 40.0, 13.0, CardMode::Link, true, true, true, true, LONG_TITLE).play.is_none(),
            "a collapsed row draws no picture"
        );
    }

    /// A video card is a poster with a two-line caption, and nothing else.
    ///
    /// *"for youtube thumbnails there should only be 2 small lines of text, the rest should be
    /// the thumbnail, and the 2 lines should be bolded."*
    ///
    /// The picture is sized from what the caption needs rather than from
    /// `CardMode::image_fraction`, which is the only arrangement where that sentence is true at
    /// every card size — at a fixed 62% the text block is three title lines and a blurb on a
    /// tall card and one clipped line on a short one.
    #[test]
    fn a_video_card_is_mostly_poster_with_its_caption_at_the_foot() {
        // Tall enough that an *ordinary* card still has room for a blurb after its three title
        // lines — otherwise the control below passes for the wrong reason, having no blurb
        // because the card is small rather than because it is a video.
        let (w, h, font) = (320.0, 600.0, card_font_size(320.0));
        let video = card_layout(w, h, font, CardMode::Large, true, true, true, true, LONG_TITLE);
        let ordinary = card_layout(w, h, font, CardMode::Large, true, true, true, false, LONG_TITLE);

        let (_, _, _, video_image) = video.image.expect("a poster");
        let (_, _, _, plain_image) = ordinary.image.expect("a picture");
        assert!(
            video_image > plain_image,
            "a video's poster should take the room its blurb gave up: {video_image:.0} \
             against {plain_image:.0}"
        );
        assert!(
            video_image / h > 0.7,
            "'make the image thumbnail very big' — the poster is {:.0}% of the card",
            video_image / h * 100.0
        );

        // One line of title, ellipsised past it, and no blurb at all.
        let line = font * TITLE_SCALE * CARD_LINE_HEIGHT;
        assert!(
            (video.title.3 - line * VIDEO_TITLE_LINES).abs() < 0.5,
            "expected {VIDEO_TITLE_LINES} title line(s) ({:.1}), got {:.1}",
            line * VIDEO_TITLE_LINES,
            video.title.3
        );
        // …and the caption ends at the foot of the card rather than floating under the
        // picture: *"put the writing all the way down"*. Within a pad of the bottom edge.
        let foot = video.title.1 + video.title.3;
        assert!(
            (h - foot) <= w * CARD_PADDING + 1.0,
            "the caption stops {:.0} short of the card's foot",
            h - foot
        );
        assert_eq!(video.blurb.3, 0.0, "a video card has no blurb");
        assert!(ordinary.blurb.3 > 0.0, "an ordinary card still does");

        // …and the blurb text is not produced either, because a 0-height box still shapes:
        // `FitBox::new` clamps to 1.0, so an empty box would draw one clipped line.
        let kind = ItemKind::LinkPreview {
            title: Some("I built an ADVANCED Battery Bank".into()),
            url: Some("https://www.youtube.com/watch?v=abc".into()),
            description: Some("Get the sponsor's app here: https://exmpl.co/07-Abcd".into()),
            provider: Some("YouTube".into()),
            thumbnail: Some("hash".into()),
            favicon: None,
            mode: CardMode::Large,
        };
        assert!(
            card_text(&kind, CARD_BLURB_SLOT, usize::MAX).is_empty(),
            "a video card's sponsor read must not be drawn"
        );
    }

    /// Which hosts count as video, and — more importantly — which do not.
    #[test]
    fn the_video_hosts_are_the_ones_that_play_something() {
        for url in [
            "https://www.youtube.com/watch?v=abc",
            "https://youtu.be/abc",
            "https://m.youtube.com/watch?v=abc",
            "https://vimeo.com/12345",
        ] {
            assert!(vellum_link::plays_video(url), "{url} is a video");
        }
        for url in [
            "https://www.alibaba.com/product-detail/x.html",
            "https://github.com/rust-lang/rust",
            "https://www.amazon.com/dp/B01",
            "not-a-url",
        ] {
            assert!(!vellum_link::plays_video(url), "{url} is not a video");
        }
    }

    /// The site's own name comes off the title, at either end.
    ///
    /// Both examples are the reference screenshots: *"Amazon.com : Superbat 3G/6G/12G SDI
    /// Cable"* and *"IQL-IMX678/FF | DigiKey Electronics"*, on cards whose provider row was
    /// already saying Amazon and DigiKey beside the site's own icon.
    #[test]
    fn a_title_does_not_repeat_the_site_the_row_above_names() {
        let strip = |title, provider| strip_site_affix(title, Some(provider));
        assert_eq!(
            strip("Amazon.com : Superbat 3G/6G/12G SDI Cable 2ft", "Amazon"),
            "Superbat 3G/6G/12G SDI Cable 2ft"
        );
        assert_eq!(strip("IQL-IMX678/FF | DigiKey Electronics", "DigiKey"), "IQL-IMX678/FF");
        assert_eq!(strip("Sony Imx678 Camera – Sincerefirst", "Sincerefirst"), "Sony Imx678 Camera");
    }

    /// …and the two guards that stop it eating a real title.
    ///
    /// A suppressed prefix is invisible when it is wrong — the card just shows a slightly
    /// shorter title — so these are the assertions that matter. Both name a case a plain
    /// `strip_prefix` fails.
    #[test]
    fn stripping_the_site_name_never_eats_the_title_itself() {
        assert_eq!(
            strip_site_affix("Amazonian Fish Species", Some("Amazon")),
            "Amazonian Fish Species",
            "there is no separator, so nothing was the site's name"
        );
        assert_eq!(
            strip_site_affix("DigiKey", Some("DigiKey")),
            "DigiKey",
            "a title that is only the site name keeps it — an empty title is worse"
        );
        assert_eq!(
            strip_site_affix("Alibaba.com : A", Some("Alibaba")),
            "Alibaba.com : A",
            "what is left is too short to be a title"
        );
        assert_eq!(
            strip_site_affix("Untitled", None),
            "Untitled",
            "no provider, nothing to compare against"
        );
    }

    /// **The inputs that used to abort the process**, found by an adversarial review rather
    /// than by the tests that shipped with the function.
    ///
    /// `[profile.release]` sets `panic = "abort"` and this runs inside `Painter::block`'s
    /// layout closure, so neither of these threw — they killed the app on the frame a card
    /// became visible, and again on relaunch, because a board reopens at the same camera.
    #[test]
    fn a_title_that_used_to_abort_the_painter_is_merely_left_alone() {
        // A provider containing a separator. `provider_for` capitalises the registrable domain
        // label when the host is not in its table, and a hyphen is legal in one — so
        // `acme-parts.com` really does yield `Acme-parts`. The separator inside the provider is
        // then the *first* match in the title, and `trimmed[9..3]` is a reversed range.
        assert_eq!(
            strip_site_affix("Acme-parts.com | Genuine Acme Parts", Some("Acme-parts")),
            "Acme-parts.com | Genuine Acme Parts",
            "left whole — nothing after a separator is the provider — and, crucially, no abort"
        );
        assert_eq!(strip_site_affix("Acme-parts", Some("Acme-parts")), "Acme-parts");
        for separator in [':', '|', '-', '\u{2013}', '\u{2014}', '\u{00BB}'] {
            let provider = format!("A{separator}B");
            let title = format!("{provider}.com : Something Worth Reading");
            let _ = strip_site_affix(&title, Some(&provider));
        }

        // A title whose byte at `provider.len()` is a continuation byte. `Alibaba` is 7 bytes
        // and a CJK character is 3, so a title of Chinese product text puts one at index 7 —
        // and this board is largely Alibaba.
        assert_eq!(
            strip_site_affix("汽车排气管 - Alibaba", Some("Alibaba")),
            "汽车排气管",
            "the trailing branch works on multibyte text"
        );
        for title in ["日本語のタイトル", "汽", "Ω≈ç√∫˜µ", "🚗🚗🚗 : Cars"] {
            for provider in ["Alibaba", "A", "汽车", "🚗"] {
                let _ = strip_site_affix(title, Some(provider));
            }
        }
    }

    /// A degenerate card must not produce negative or NaN boxes — a zero-sized item is
    /// reachable by dragging a resize handle onto itself.
    #[test]
    fn a_zero_sized_card_still_lays_out() {
        for mode in CardMode::ALL {
            let laid = card_layout(0.0, 0.0, 13.0, mode, true, true, true, false, LONG_TITLE);
            for (label, (x, y, w, h)) in [
                ("provider", laid.provider),
                ("body", laid.body),
            ] {
                assert!(x.is_finite() && y.is_finite(), "{label} at ({x}, {y})");
                assert!(w >= 0.0 && h >= 0.0, "{label} is {w}x{h}");
            }
            if let Some((_, _, w, h)) = laid.image {
                assert!(w > 0.0 && h > 0.0, "an image box is never zero: {w}x{h}");
            }
        }
    }

    /// The ellipsis helper's own edges: a budget too small to cut, and a word boundary.
    ///
    /// **The tiny-budget answer changed, and the old one was the bug.** This used to
    /// assert `ellipsise("hello", 1) == "hello"`, reasoned as *"no room for a cut and an
    /// ellipsis"* — true about the ellipsis and exactly backwards about the text. Every
    /// caller uses this to *bound* a string to a block, so returning more than the budget
    /// defeats the only thing it is for: a card whose title had spent its block then drew
    /// its whole 600-character URL underneath and out through the bottom, which is what
    /// *"i still have the links overflowing problem"* was looking at.
    ///
    /// Nothing for no room, a bare ellipsis for one character. Both are honest about
    /// there being more text; neither can overflow.
    #[test]
    fn ellipsising_cuts_on_a_word_and_survives_a_tiny_budget() {
        assert_eq!(ellipsise("hello world again", 12), "hello world…");
        assert_eq!(ellipsise("short", 12), "short", "nothing to cut");
        assert_eq!(ellipsise("hello", 1), "…", "one character of room is the ellipsis");
        assert_eq!(ellipsise("hello", 0), "", "no room is nothing, never everything");
        assert_eq!(ellipsise("", 0), "", "empty in, empty out");
        // A single long word has no boundary to cut on, so it is cut mid-word rather than
        // thrown away entirely.
        assert_eq!(ellipsise("aaaaaaaaaaaa", 5), "aaaa…");
        // Multi-byte: the cut lands on a character boundary.
        let cut = ellipsise("héllo wörld ägain", 8);
        assert!(cut.ends_with('…') && cut.chars().count() <= 8, "{cut:?}");
    }

    /// A card with no metadata at all — Miro serves plenty — must produce an empty
    /// block rather than the word "None".
    #[test]
    fn a_card_with_no_metadata_has_no_text() {
        let bare = ItemKind::link_preview(None, None, None);
        assert!(card_text(&bare, BlockKey::PRIMARY, usize::MAX).is_empty());
    }

    /// A table gets a text slot per cell, on top of the two every kind has. Without
    /// this the loop stops at 2 and a table draws its grid with nothing in it — which
    /// is exactly what it did before the slot count became per-kind.
    #[test]
    fn a_table_claims_a_text_slot_for_every_cell() {
        let table = crate::table::default_table();
        let projection = projection_with([NewItem::new(
            ItemKind::Table { model: crate::table::encode(&table) },
            Placement::new(0.0, 0.0, 480.0, 220.0),
        )]);
        let (&id, projected) = projection.iter().next().unwrap();
        let mut painter = painter();
        let camera = camera_at(1.0);
        let ctx = context(&camera, &projection);

        let slots = painter.slots_of(id, projected, ctx.projection.generation(), &ctx);
        let cells = crate::table::DEFAULT_ROWS * crate::table::DEFAULT_COLUMNS;
        assert_eq!(usize::from(slots), 2 + cells, "a 3x3 table needs 9 cell slots");

        // A sticky beside it still claims only its two, so the count is per kind
        // rather than a blanket widening.
        let plain = projection_with([NewItem::new(
            ItemKind::Sticky { text: StyledText::plain("hi"), background: None },
            Placement::new(0.0, 0.0, 200.0, 200.0),
        )]);
        let (&sid, sprojected) = plain.iter().next().unwrap();
        let sctx = context(&camera, &plain);
        assert_eq!(painter.slots_of(sid, sprojected, sctx.projection.generation(), &sctx), 2);
    }

    /// Cells with words produce blocks; empty cells produce none, so a freshly placed
    /// table costs nothing to shape.
    #[test]
    fn only_the_cells_with_words_are_shaped() {
        use vellum_table::{CellRef, StyledText as TableText};
        let mut table = crate::table::default_table();
        table.set_content(CellRef::new(0, 0), TableText::plain("Item")).unwrap();
        table.set_content(CellRef::new(1, 1), TableText::plain("2")).unwrap();

        let projection = projection_with([NewItem::new(
            ItemKind::Table { model: crate::table::encode(&table) },
            Placement::new(0.0, 0.0, 480.0, 220.0),
        )]);
        let (&id, projected) = projection.iter().next().unwrap();
        let mut painter = painter();
        let camera = camera_at(1.0);
        let ctx = context(&camera, &projection);

        let slots = painter.slots_of(id, projected, ctx.projection.generation(), &ctx);
        let drawn = (2..slots)
            .filter(|slot| painter.block(id, projected, *slot, &ctx).is_some())
            .count();
        assert_eq!(drawn, 2, "two filled cells should give two blocks");
    }

    /// A mind map gets a text slot per visible node, and every node in the default map
    /// has a label — so all nine shape. Without the per-kind count the loop stops at 2
    /// and a map draws its boxes and branches with no words in them.
    #[test]
    fn a_mind_map_claims_a_text_slot_for_every_node() {
        let model = crate::mindmap::default_mindmap();
        let nodes = model.map.node_count();
        let projection = projection_with([NewItem::new(
            ItemKind::MindMap { model: crate::mindmap::encode(&model) },
            Placement::new(0.0, 0.0, crate::mindmap::DEFAULT_SIZE.0, crate::mindmap::DEFAULT_SIZE.1),
        )]);
        let (&id, projected) = projection.iter().next().unwrap();
        let mut painter = painter();
        let camera = camera_at(1.0);
        let ctx = context(&camera, &projection);

        let slots = painter.slots_of(id, projected, ctx.projection.generation(), &ctx);
        assert_eq!(usize::from(slots), 2 + nodes, "a nine-node map needs nine node slots");

        let drawn = (2..slots)
            .filter(|slot| painter.block(id, projected, *slot, &ctx).is_some())
            .count();
        assert_eq!(drawn, nodes, "every node in the default map is labelled");
    }

    /// A collapsed branch is not laid out, so it claims no slots and shapes no text —
    /// the thing that makes folding a large map cheap rather than merely tidy.
    #[test]
    fn a_folded_branch_costs_no_slots() {
        let mut model = crate::mindmap::default_mindmap();
        let root = model.map.root();
        let branch = model.map.children(root)[0];
        let hidden = 1 + model.map.descendant_count(branch);
        model.map.set_collapsed(branch, true).unwrap();

        let projection = projection_with([NewItem::new(
            ItemKind::MindMap { model: crate::mindmap::encode(&model) },
            Placement::new(0.0, 0.0, 520.0, 260.0),
        )]);
        let (&id, projected) = projection.iter().next().unwrap();
        let mut painter = painter();
        let camera = camera_at(1.0);
        let ctx = context(&camera, &projection);

        // The branch itself stays visible; its two children do not.
        let slots = painter.slots_of(id, projected, ctx.projection.generation(), &ctx);
        assert_eq!(usize::from(slots), 2 + 9 - (hidden - 1), "folding freed no slots");
    }

    /// Resizing the item scales the map rather than re-flowing it, and the scale is
    /// uniform: a map stretched to a dragged box would put its text at one aspect and
    /// its branches at another. Halving the box halves the scale.
    #[test]
    fn resizing_a_mind_map_scales_it_uniformly() {
        let model = crate::mindmap::default_mindmap();
        let projection = projection_with([NewItem::new(
            ItemKind::MindMap { model: crate::mindmap::encode(&model) },
            Placement::new(0.0, 0.0, 520.0, 260.0),
        )]);
        let (&id, projected) = projection.iter().next().unwrap();
        let mut painter = painter();
        let camera = camera_at(1.0);
        let ctx = context(&camera, &projection);

        let cached = painter.mindmap_layout(id, projected, ctx.projection.generation());
        let (nw, nh) = cached.natural;
        assert!((cached.scale((nw, nh)) - 1.0).abs() < 1e-9, "the natural box is 1:1");
        assert!((cached.scale((nw / 2.0, nh / 2.0)) - 0.5).abs() < 1e-9);
        // The smaller ratio wins, so a box wide in one axis only does not stretch.
        assert!((cached.scale((nw * 4.0, nh / 2.0)) - 0.5).abs() < 1e-9);
        // A degenerate box falls back to 1 rather than to zero or NaN, so a map that
        // somehow lost its size still draws instead of vanishing.
        assert!((cached.scale((0.0, 0.0)) - 1.0).abs() < 1e-9);
    }

    /// A stroke in flight exists only between the press and the release, which is
    /// exactly the window `--screenshot` cannot photograph — it renders one frame of a
    /// board nobody is touching. So the check that the pen draws *while* it is drawing
    /// has to live here.
    #[test]
    fn a_stroke_in_flight_draws_before_it_is_ever_an_item() {
        let projection = projection_with([]);
        let camera = Camera::new(ScreenSize::new(1600.0, 900.0));
        let mut list = DrawList::new();
        let board = list.view(View::board(&camera));

        let points = [
            WorldPoint::new(10.0, 10.0),
            WorldPoint::new(40.0, 25.0),
            WorldPoint::new(70.0, 60.0),
        ];
        let ctx = DrawContext {
            agents: crate::agent_view::AgentViews::empty(),
            camera: &camera,
            projection: &projection,
            theme: Theme::LIGHT,
            selection: &[],
            hovered_badge: None,
            ports: None,
            connector_grips: None,
            default_chat_theme: vellum_agent::ChatTheme::Velm,
            default_display: DisplayMode::default(),
            marquee: None,
            placing: None,
            guides: &[],
            stroke: Some(LiveStroke { points: &points, color: Rgba::BLACK, thickness: 4.0 }),
            pending_connector: None,
            editing: None,
            card_drop: None,
            pattern: Pattern::Plain,
            grid_color: None,
            territories: Vec::new(),
            minimap: None,
        };
        push_stroke(&mut list, &ctx, board);

        // The document is empty — every triangle here belongs to the uncommitted path.
        assert!(list.meshes().indices().len() >= 3, "the stroke tessellated to nothing");
        assert_eq!(list.stats().draw_calls, 1, "one mesh batch, in the board view");
    }

    /// The floor `commit_stroke` applies, applied to the preview too: a press that has
    /// not travelled must not flash a mark that will never be kept.
    #[test]
    fn a_single_sample_previews_nothing() {
        let projection = projection_with([]);
        let camera = Camera::new(ScreenSize::new(1600.0, 900.0));
        let mut list = DrawList::new();
        let board = list.view(View::board(&camera));

        let points = [WorldPoint::new(10.0, 10.0)];
        let ctx = DrawContext {
            agents: crate::agent_view::AgentViews::empty(),
            camera: &camera,
            projection: &projection,
            theme: Theme::LIGHT,
            selection: &[],
            hovered_badge: None,
            ports: None,
            connector_grips: None,
            default_chat_theme: vellum_agent::ChatTheme::Velm,
            default_display: DisplayMode::default(),
            marquee: None,
            placing: None,
            guides: &[],
            stroke: Some(LiveStroke { points: &points, color: Rgba::BLACK, thickness: 4.0 }),
            pending_connector: None,
            editing: None,
            card_drop: None,
            pattern: Pattern::Plain,
            grid_color: None,
            territories: Vec::new(),
            minimap: None,
        };
        push_stroke(&mut list, &ctx, board);

        assert_eq!(list.meshes().indices().len(), 0);
        assert_eq!(list.stats().draw_calls, 0);
    }

    /// Vertices are f32 and only safe in stroke-local space. A stroke drawn far from
    /// the origin must still tessellate about its own first point, or the mesh carries
    /// the board's whole extent and shimmers — and it must land in the same place the
    /// committed item will, which is what stops a stroke jumping on mouse-up.
    #[test]
    fn a_stroke_far_from_the_origin_keeps_its_vertices_small() {
        let projection = projection_with([]);
        let camera = Camera::new(ScreenSize::new(1600.0, 900.0));
        let mut list = DrawList::new();
        let board = list.view(View::board(&camera));

        let far = 41_282.0;
        let points = [
            WorldPoint::new(far, far),
            WorldPoint::new(far + 30.0, far + 15.0),
            WorldPoint::new(far + 60.0, far + 50.0),
        ];
        let ctx = DrawContext {
            agents: crate::agent_view::AgentViews::empty(),
            camera: &camera,
            projection: &projection,
            theme: Theme::LIGHT,
            selection: &[],
            hovered_badge: None,
            ports: None,
            connector_grips: None,
            default_chat_theme: vellum_agent::ChatTheme::Velm,
            default_display: DisplayMode::default(),
            marquee: None,
            placing: None,
            guides: &[],
            stroke: Some(LiveStroke { points: &points, color: Rgba::BLACK, thickness: 4.0 }),
            pending_connector: None,
            editing: None,
            card_drop: None,
            pattern: Pattern::Plain,
            grid_color: None,
            territories: Vec::new(),
            minimap: None,
        };
        push_stroke(&mut list, &ctx, board);

        let bound = 200.0;
        for vertex in list.meshes().vertices() {
            let [x, y] = vertex.position;
            assert!(
                x.abs() < bound && y.abs() < bound,
                "vertex ({x}, {y}) carries the board's extent, not the stroke's",
            );
        }
    }

    #[test]
    fn a_marquee_is_drawn_in_screen_pixels_whichever_way_it_was_dragged() {
        let projection = projection_with([]);
        let camera = Camera::new(ScreenSize::new(1600.0, 900.0));
        let mut list = DrawList::new();
        let screen = list.view(View::screen(camera.viewport()));

        let ctx = DrawContext {
            agents: crate::agent_view::AgentViews::empty(),
            camera: &camera,
            projection: &projection,
            theme: Theme::LIGHT,
            selection: &[],
            hovered_badge: None,
            ports: None,
            connector_grips: None,
            default_chat_theme: vellum_agent::ChatTheme::Velm,
            default_display: DisplayMode::default(),
            marquee: Some((ScreenPoint::new(400.0, 300.0), ScreenPoint::new(100.0, 100.0))),
            placing: None,
            guides: &[],
            stroke: None,
            pending_connector: None,
            editing: None,
            card_drop: None,
            pattern: Pattern::Plain,
            grid_color: None,
            territories: Vec::new(),
            minimap: None,
        };
        push_marquee(&mut list, &ctx, screen);

        assert_eq!(list.stats().quads, 1);
        assert_eq!(list.stats().draw_calls, 1);
    }

    /// *"when i am trying to draw a frame i do not see it as i draw … it just spawns."*
    ///
    /// A placing drag writes nothing to the document until the button comes up, so the
    /// preview is the only thing that can put the gesture on screen. Every look draws —
    /// including [`PlacingLook::Shape`], where the point is that it goes down the SDF path
    /// rather than the quad one, and the ghost fallback, where the point is that a form the
    /// SDF cannot express still draws *something*: an empty preview is the reported bug.
    #[test]
    fn every_placing_look_puts_the_gesture_on_screen() {
        let projection = projection_with([]);
        let camera = Camera::new(ScreenSize::new(1600.0, 900.0));

        for look in [
            PlacingLook::Frame,
            PlacingLook::Sticky,
            PlacingLook::Shape(Shape::Ellipse),
            PlacingLook::Ghost,
        ] {
            let mut list = DrawList::new();
            let board = list.view(View::board(&camera));
            let ctx = DrawContext {
                agents: crate::agent_view::AgentViews::empty(),
                camera: &camera,
                projection: &projection,
                theme: Theme::LIGHT,
                selection: &[],
                hovered_badge: None,
            ports: None,
            connector_grips: None,
            default_chat_theme: vellum_agent::ChatTheme::Velm,
            default_display: DisplayMode::default(),
                marquee: None,
                guides: &[],
                placing: Some(Placing {
                    placement: Placement::new(0.0, 0.0, 400.0, 200.0),
                    look,
                }),
                stroke: None,
                pending_connector: None,
                editing: None,
                card_drop: None,
                pattern: Pattern::Plain,
                grid_color: None,
                territories: Vec::new(),
                minimap: None,
            };
            push_placing(&mut list, &ctx, board);

            let stats = list.stats();
            assert!(
                stats.quads + stats.shapes > 0,
                "{look:?} drew nothing, which is the bug this exists to fix",
            );
            // The accent outline that says *this is a gesture, not an item yet* is on top
            // of every one of them, so a look that drew only its own fill is a miss.
            assert!(stats.quads >= 1, "{look:?} has no outline: {stats:?}");
        }
    }

    /// A guide is **dashed**, and a guide that runs off the world is still bounded.
    ///
    /// *"they are too bright, so make those into dotted dashed lines and turn transparency
    /// down a bit so i can see the difference between the alignment line and an object."*
    /// The first half is what this asserts — one quad is a solid line, and a solid accent
    /// hairline is exactly what a selection ring and a shape's border already are.
    ///
    /// The second assertion is the one that would not have been written without looking:
    /// a guide's span is in **world** units, so two items far apart on a zoomed-in board
    /// give a segment that is mostly off screen. Dashing it unclipped is a quad per nine
    /// pixels of a line nobody can see.
    #[test]
    fn a_guide_is_dashed_and_clipped_to_the_window() {
        let projection = projection_with([]);
        let camera = camera_at(1.0);

        let count = |guide: crate::snap::Guide| {
            let mut list = DrawList::new();
            let screen = list.view(View::screen(camera.viewport()));
            let mut ctx = context(&camera, &projection);
            let guides = [guide];
            ctx.guides = &guides;
            push_guides(&mut list, &ctx, screen);
            list.stats().quads
        };

        // A guide across most of the window: many dashes, not one bar.
        let across = count(crate::snap::Guide {
            axis: crate::snap::Axis::Vertical,
            at: 0.0,
            from: -400.0,
            to: 400.0,
            gap: None,
        });
        assert!(across > 10, "a dashed guide is a run of quads, not one bar: {across}");

        // The same line, spanning a hundred thousand world units. The window has not grown,
        // so neither may the quad count.
        let enormous = count(crate::snap::Guide {
            axis: crate::snap::Axis::Vertical,
            at: 0.0,
            from: -50_000.0,
            to: 50_000.0,
            gap: None,
        });
        // Bounded by the **window**, not by the shorter guide — a line that does cross the
        // whole window legitimately needs more dashes than one that stops inside it. Derived
        // from the constants rather than written down, so changing the dash pattern does not
        // silently turn this into a assertion about nothing.
        let full_window =
            (camera.viewport().height as f32 / (GUIDE_DASH + GUIDE_GAP)).ceil() as usize + 2;
        assert!(
            enormous <= full_window,
            "a guide 100k units long drew {enormous} quads; a full window needs {full_window} \
             (one screenful of guide drew {across})",
        );
        assert!(enormous < GUIDE_MAX_DASHES, "the clip did nothing and the backstop caught it");
    }

    #[test]
    fn no_marquee_draws_nothing() {
        let projection = projection_with([]);
        let camera = Camera::new(ScreenSize::new(1600.0, 900.0));
        let mut list = DrawList::new();
        let screen = list.view(View::screen(camera.viewport()));
        push_marquee(
            &mut list,
            &DrawContext {
                agents: crate::agent_view::AgentViews::empty(),
                camera: &camera,
                projection: &projection,
                theme: Theme::LIGHT,
                selection: &[],
                hovered_badge: None,
            ports: None,
            connector_grips: None,
            default_chat_theme: vellum_agent::ChatTheme::Velm,
            default_display: DisplayMode::default(),
                marquee: None,
                placing: None,
                guides: &[],
                stroke: None,
                pending_connector: None,
                editing: None,
                card_drop: None,
                pattern: Pattern::Plain,
                grid_color: None,
                territories: Vec::new(),
                minimap: None,
            },
            screen,
        );
        // The view itself is still declared — the frame has one whether or not the
        // marquee uses it — so it is the geometry that has to be empty.
        assert_eq!(list.stats().quads, 0);
        assert_eq!(list.stats().draw_calls, 0);
    }

    /// A selection ring is chrome: it has to hold its screen thickness as the board
    /// is zoomed, or it becomes a slab at 64× and invisible at 1%.
    #[test]
    fn a_selection_ring_holds_its_screen_width_at_any_zoom() {
        let projection = projection_with([NewItem::new(
            ItemKind::Sticky { text: StyledText::default(), background: None },
            Placement::new(0.0, 0.0, 200.0, 200.0),
        )]);
        let id = *projection.iter().next().unwrap().0;
        let painter = Painter::new(TextCache::with_fonts([]).unwrap_or_else(|_| {
            TextCache::new().expect("the test machine has fonts")
        }));

        for zoom in [0.25f64, 1.0, 16.0] {
            let mut camera = Camera::new(ScreenSize::new(1600.0, 900.0));
            camera.set_zoom_about(zoom, ScreenPoint::new(800.0, 450.0));
            let mut list = DrawList::new();
            let board = list.view(View::board(&camera));
            painter.push_selection(
                &mut list,
                &DrawContext {
                    agents: crate::agent_view::AgentViews::empty(),
            hovered_badge: None,
            ports: None,
            connector_grips: None,
            default_chat_theme: vellum_agent::ChatTheme::Velm,
            default_display: DisplayMode::default(),
                    camera: &camera,
                    projection: &projection,
                    theme: Theme::LIGHT,
                    selection: &[id],
                    marquee: None,
                    placing: None,
                    guides: &[],
                    stroke: None,
                    pending_connector: None,
                    editing: None,
                    card_drop: None,
                    pattern: Pattern::Plain,
                    grid_color: None,
                    territories: Vec::new(),
                    minimap: None,
                },
                board,
            );
            // The ring, plus a handle apiece for eight edges and corners and the
            // rotate handle above the top edge.
            assert_eq!(list.stats().quads, 1 + crate::handle::Handle::ALL.len(), "zoom {zoom}");
        }
    }

    /// Handles are chrome: a constant size on screen at every zoom, like the ring they
    /// sit on. Drawn in the board view — so a rotated item's handles turn with it — but
    /// with every dimension divided by the zoom.
    #[test]
    fn handles_hold_their_screen_size_at_any_zoom() {
        let projection = projection_with([NewItem::new(
            ItemKind::Sticky { text: StyledText::default(), background: None },
            Placement::new(0.0, 0.0, 200.0, 200.0),
        )]);
        let id = *projection.iter().next().unwrap().0;

        let mut sizes = Vec::new();
        for zoom in [0.25f64, 1.0, 16.0] {
            let mut camera = Camera::new(ScreenSize::new(1600.0, 900.0));
            camera.set_zoom_about(zoom, ScreenPoint::new(800.0, 450.0));
            let mut list = DrawList::new();
            let board = list.view(View::board(&camera));
            let ctx = DrawContext {
                agents: crate::agent_view::AgentViews::empty(),
            hovered_badge: None,
            ports: None,
            connector_grips: None,
            default_chat_theme: vellum_agent::ChatTheme::Velm,
            default_display: DisplayMode::default(),
                camera: &camera,
                projection: &projection,
                theme: Theme::LIGHT,
                selection: &[id],
                marquee: None,
                placing: None,
                guides: &[],
                stroke: None,
                pending_connector: None,
                editing: None,
                card_drop: None,
                pattern: Pattern::Plain,
                grid_color: None,
                territories: Vec::new(),
                minimap: None,
            };
            push_handles(&mut list, &ctx, board);
            assert_eq!(list.stats().quads, crate::handle::Handle::ALL.len(), "zoom {zoom}");
            // World size × zoom is the size in device pixels, which must not move.
            let world = f64::from(crate::handle::HANDLE_SIZE) / zoom;
            sizes.push((world * zoom).round() as i64);
        }
        assert!(sizes.windows(2).all(|w| w[0] == w[1]), "handles changed size on screen: {sizes:?}");
    }

    /// A multi-selection gets **five** handles on a shared box — four corners and rotate —
    /// plus the box's own four hairlines. This test used to assert *zero* quads, back when
    /// handles belonged to one item; the group transform is a genuinely different operation
    /// (every member's size *and* centre move, and a rotation moves each member's angle as
    /// well) rather than a bigger version of the single one, which is why it took its own
    /// geometry in `handle`.
    ///
    /// **No edge handles**, and that is the load-bearing assertion: an edge drag scales one
    /// axis, and one-axis scaling of a rotated member is a shear, which no `Placement` can
    /// express.
    #[test]
    fn a_multi_selection_gets_handles_on_a_shared_box() {
        let projection = projection_with([
            NewItem::new(
                ItemKind::Sticky { text: StyledText::default(), background: None },
                Placement::new(0.0, 0.0, 100.0, 100.0),
            ),
            NewItem::new(
                ItemKind::Sticky { text: StyledText::default(), background: None },
                Placement::new(400.0, 0.0, 100.0, 100.0),
            ),
        ]);
        let ids: Vec<_> = projection.iter().map(|(id, _)| *id).collect();
        let camera = Camera::new(ScreenSize::new(1600.0, 900.0));
        let mut list = DrawList::new();
        let board = list.view(View::board(&camera));
        let ctx = DrawContext {
            agents: crate::agent_view::AgentViews::empty(),
            hovered_badge: None,
            ports: None,
            connector_grips: None,
            default_chat_theme: vellum_agent::ChatTheme::Velm,
            default_display: DisplayMode::default(),
            camera: &camera,
            projection: &projection,
            theme: Theme::LIGHT,
            selection: &ids,
            marquee: None,
            placing: None,
            guides: &[],
            stroke: None,
            pending_connector: None,
            editing: None,
            card_drop: None,
            pattern: Pattern::Plain,
            grid_color: None,
            territories: Vec::new(),
            minimap: None,
        };
        push_handles(&mut list, &ctx, board);
        // Four outline hairlines plus five handles.
        assert_eq!(list.stats().quads, 4 + crate::handle::GROUP_HANDLES.len());

        // And the box spans both stickies, so the handles are on its corners rather than on
        // either member's.
        let placements: Vec<Placement> = ids
            .iter()
            .filter_map(|id| projection.get(*id))
            .map(|projected| projected.item.placement)
            .collect();
        let group = crate::handle::group_bounds(&placements).expect("two items have a box");
        assert!((group.width - 500.0).abs() < 1e-9, "width {}", group.width);
        assert!((group.x - 200.0).abs() < 1e-9, "centre {}", group.x);
    }

    /// A block that loses the shaping ration draws **bars, not nothing**.
    ///
    /// This is the user-visible half of the edit-speed work. `TEXT_LAYOUT_BUDGET` rations
    /// shaping to 3 ms a frame and used to answer `None` past it — no glyphs, no bars, a
    /// hole. Since an edit invalidated every layout on the board (see
    /// `Projection::rebuild`), that meant the words vanished after every edit and undo and
    /// came back over dozens of frames.
    ///
    /// **Why the budget is spent by hand rather than by shaping 3 ms of real text.** A
    /// test that raced the clock would pass or fail with the machine's load — the same
    /// thing that makes `glass_budget` unreliable — and would take 3 ms of wall time to
    /// say so. Setting `text_spent` directly is the same state the ration produces, with
    /// no clock in the assertion.
    ///
    /// A/B'd: with the starved case restored to `return None` the first assertion fails
    /// with *"a starved block drew nothing"*.
    #[test]
    fn a_block_that_runs_out_of_shaping_budget_draws_bars_rather_than_nothing() {
        let projection = projection_with([NewItem::new(
            ItemKind::Sticky {
                text: StyledText::plain("words the user is waiting for"),
                background: None,
            },
            Placement::new(0.0, 0.0, 400.0, 400.0),
        )]);
        let (id, projected) = projection.iter().next().expect("one sticky");
        // A zoom at which the text is comfortably readable, so the too-small guard is not
        // what produces the bars — otherwise this would pass on a build with the fix
        // removed, which is the mistake `--demo grid-snap` already made once.
        let camera = Camera::new(ScreenSize::new(1600.0, 900.0));
        assert!(
            largest_font_size(&Style::default(), FitBox::new(400.0, 400.0)) * camera.zoom()
                >= f64::from(MIN_DEVICE_FONT_SIZE),
            "the fixture is too small to read, so it would greek either way"
        );
        let ctx = DrawContext {
            agents: crate::agent_view::AgentViews::empty(),
            hovered_badge: None,
            ports: None,
            connector_grips: None,
            default_chat_theme: vellum_agent::ChatTheme::Velm,
            default_display: DisplayMode::default(),
            camera: &camera,
            projection: &projection,
            theme: Theme::LIGHT,
            selection: &[],
            marquee: None,
            placing: None,
            guides: &[],
            stroke: None,
            pending_connector: None,
            editing: None,
            card_drop: None,
            pattern: Pattern::Plain,
            grid_color: None,
            territories: Vec::new(),
            minimap: None,
        };

        let mut painter = Painter::new(TextCache::new().expect("the test machine has fonts"));
        painter.text_spent = TEXT_LAYOUT_BUDGET;
        let starved = painter.block(*id, projected, 0, &ctx);
        assert!(
            matches!(starved, Some(Painted::Greeked(_))),
            "a starved block drew nothing: {starved:?}"
        );
        assert_eq!(painter.text_deferred, 1, "the starved block was not counted");
        assert_eq!(painter.text.len(), 0, "a starved block shaped anyway, which is the cost");

        // And with budget, the same block shapes for real — so the bars are the *waiting*
        // state and not a permanent downgrade.
        painter.text_spent = Duration::ZERO;
        let shaped = painter.block(*id, projected, 0, &ctx);
        assert!(matches!(shaped, Some(Painted::Glyphs(_))), "a block with budget drew bars");
    }

    /// The caret's block is shaped whatever the ration says.
    ///
    /// Greeking the block being typed into would be worse than the bug this fixes: bars
    /// have no glyph positions, so there is no origin to draw the cursor against and the
    /// caret disappears while the user is typing. Feedback 25 arrived at the same rule
    /// from the empty-sticky direction.
    #[test]
    fn the_block_holding_the_caret_is_never_starved() {
        let projection = projection_with([NewItem::new(
            ItemKind::Sticky { text: StyledText::plain("typing here"), background: None },
            Placement::new(0.0, 0.0, 400.0, 400.0),
        )]);
        let (id, projected) = projection.iter().next().expect("one sticky");
        let camera = Camera::new(ScreenSize::new(1600.0, 900.0));
        let editing = TextCursor {
            scene: *id,
            slot: 0,
            idle_for: 0.0,
            cursor: 0,
            anchor: 0,
            text: "typing here",
        };
        let ctx = DrawContext {
            agents: crate::agent_view::AgentViews::empty(),
            hovered_badge: None,
            ports: None,
            connector_grips: None,
            default_chat_theme: vellum_agent::ChatTheme::Velm,
            default_display: DisplayMode::default(),
            camera: &camera,
            projection: &projection,
            theme: Theme::LIGHT,
            selection: &[],
            marquee: None,
            placing: None,
            guides: &[],
            stroke: None,
            pending_connector: None,
            editing: Some(editing),
            card_drop: None,
            pattern: Pattern::Plain,
            grid_color: None,
            territories: Vec::new(),
            minimap: None,
        };

        let mut painter = Painter::new(TextCache::new().expect("the test machine has fonts"));
        painter.text_spent = TEXT_LAYOUT_BUDGET;
        let painted = painter.block(*id, projected, 0, &ctx);
        assert!(
            matches!(painted, Some(Painted::Glyphs(_))),
            "the caret's own block was greeked, so the cursor has nothing to sit against"
        );
    }

    #[test]
    fn an_unknown_selected_id_is_skipped_rather_than_panicking() {
        let projection = projection_with([]);
        let camera = Camera::new(ScreenSize::new(1600.0, 900.0));
        let painter = Painter::new(TextCache::new().expect("the test machine has fonts"));
        let mut list = DrawList::new();
        let board = list.view(View::board(&camera));

        painter.push_selection(
            &mut list,
            &DrawContext {
                agents: crate::agent_view::AgentViews::empty(),
            hovered_badge: None,
            ports: None,
            connector_grips: None,
            default_chat_theme: vellum_agent::ChatTheme::Velm,
            default_display: DisplayMode::default(),
                camera: &camera,
                projection: &projection,
                theme: Theme::LIGHT,
                selection: &[42],
                marquee: None,
                placing: None,
                guides: &[],
                stroke: None,
                pending_connector: None,
                editing: None,
                card_drop: None,
                pattern: Pattern::Plain,
                grid_color: None,
                territories: Vec::new(),
                minimap: None,
            },
            board,
        );
        assert_eq!(list.stats().quads, 0);
    }

    /// Layouts and tessellated ink are the two things that survive between frames, so
    /// they are also the two things that leak if a deleted item is never forgotten.
    #[test]
    fn deleting_an_item_retires_what_was_cached_for_it() {
        let mut board = Board::new();
        let doomed = board
            .add(NewItem::new(
                ItemKind::Sticky { text: StyledText::plain("fan"), background: None },
                Placement::new(0.0, 0.0, 400.0, 400.0),
            ))
            .unwrap();
        let mut projection = Projection::new();
        projection.rebuild(&board).unwrap();

        let mut painter = Painter::new(TextCache::new().expect("the test machine has fonts"));
        // Adopt the board before filling its caches, which is the order a frame runs
        // in: `sync` precedes `paint`. A painter that has never seen a board clears on
        // first sight, and that must not eat the entries this test is about.
        painter.sync(BoardEpoch(0), &projection);

        let id = projection.scene_id(doomed).unwrap();
        painter.text.layout(
            BlockKey::primary(id),
            projection.generation(),
            &Style::default(),
            None,
            || vellum_text::StyledText::plain("fan"),
        );
        painter.ink.insert(
            id,
            CachedInk { generation: projection.generation(), band: 0, mesh: Default::default() },
        );
        // One board throughout, so the epoch is constant and `sync` takes the prune
        // path rather than the board-switch path.
        painter.sync(BoardEpoch(0), &projection);
        assert_eq!(painter.text.len(), 1, "syncing dropped a live item");
        assert_eq!(painter.ink.len(), 1);

        board.remove(doomed).unwrap();
        projection.rebuild(&board).unwrap();
        painter.sync(BoardEpoch(0), &projection);

        assert_eq!(painter.text.len(), 0, "a deleted item's layout stayed cached");
        assert_eq!(painter.ink.len(), 0, "a deleted item's ink stayed cached");
    }

    /// Two freshly-opened boards agree on *both* halves of the old cache key: every
    /// `Projection` interns `SceneId`s from zero, and every one of them is at
    /// generation 1 because `Editor::in_memory` reprojects exactly once. So a switch
    /// between them used to skip the prune entirely and `TextCache::layout` handed the
    /// first board's words to the second board's sticky.
    #[test]
    fn switching_boards_clears_caches_even_when_generations_collide() {
        let sticky = |text: &str| {
            let mut board = Board::new();
            board
                .add(NewItem::new(
                    ItemKind::Sticky { text: StyledText::plain(text), background: None },
                    Placement::new(0.0, 0.0, 400.0, 400.0),
                ))
                .unwrap();
            let mut projection = Projection::new();
            projection.rebuild(&board).unwrap();
            projection
        };

        let (first, second) = (sticky("alpha"), sticky("omega"));
        assert_eq!(
            first.generation(),
            second.generation(),
            "the collision this test exists for did not happen"
        );

        let mut painter = Painter::new(TextCache::new().expect("the test machine has fonts"));
        painter.sync(BoardEpoch(1), &first);

        let id = *first.iter().next().expect("the board has one sticky").0;
        painter.text.layout(
            BlockKey::primary(id),
            first.generation(),
            &Style::default(),
            None,
            || vellum_text::StyledText::plain("alpha"),
        );
        painter.ink.insert(
            id,
            CachedInk { generation: first.generation(), band: 0, mesh: Default::default() },
        );
        assert_eq!(painter.text.len(), 1);

        // The switch. Retaining against `second` would keep both entries — its ids
        // cover the same range — so only clearing is correct.
        painter.sync(BoardEpoch(2), &second);

        assert_eq!(painter.text.len(), 0, "the previous board's layout crossed a switch");
        assert_eq!(painter.ink.len(), 0, "the previous board's ink crossed a switch");
    }

    #[test]
    fn a_cards_body_becomes_an_analytic_shape() {
        let mut list = DrawList::new();
        list.view(View::screen(ScreenSize::new(800.0, 600.0)));
        push_shape_card(&mut list, [0.0, 0.0], [320.0, 200.0], 0.0, 1.0, &Theme::LIGHT);
        assert_eq!(list.stats().shapes, 1);
    }

    /// A degenerate item — zero width, which an import can produce — must not make
    /// the shape parameters non-finite.
    #[test]
    fn a_zero_sized_card_does_not_produce_nan_geometry() {
        let mut list = DrawList::new();
        list.view(View::screen(ScreenSize::new(800.0, 600.0)));
        push_shape_card(&mut list, [0.0, 0.0], [0.0, 0.0], 0.0, 1.0, &Theme::LIGHT);
        assert!(list.stats().shapes <= 1);
    }

    /// The projection's own extent has to make sense before anything draws it —
    /// a "fit to content" that fits nothing is the first thing a user hits.
    #[test]
    fn content_bounds_are_usable_for_a_fit() {
        let projection = projection_with([
            NewItem::new(
                ItemKind::Sticky { text: StyledText::default(), background: None },
                Placement::new(-500.0, -300.0, 200.0, 200.0),
            ),
            NewItem::new(
                ItemKind::Sticky { text: StyledText::default(), background: None },
                Placement::new(500.0, 300.0, 200.0, 200.0),
            ),
        ]);
        let content: WorldRect = projection.content_bounds().unwrap();
        let mut camera = Camera::new(ScreenSize::new(1600.0, 900.0));
        camera.fit_to_rect(content, 0.02);
        assert_eq!(projection.scene().query_viewport(&camera).count(), 2);
    }
}

#[cfg(test)]
mod caret_blink_tests {
    use super::{CARET_BLINK_PERIOD, CARET_SOLID_FOR, caret_is_visible};

    /// Solid while typing, then blinking — the behaviour of every native text field, and
    /// the half that a naive `sin(t) > 0` gets wrong: the caret would flicker under the
    /// user's own hands during a burst of typing.
    #[test]
    fn the_caret_is_solid_while_typing_and_blinks_once_idle() {
        assert!(caret_is_visible(0.0), "invisible the instant a key lands");
        assert!(caret_is_visible(CARET_SOLID_FOR - 0.01), "blinked before the grace ended");

        // First off-phase begins as soon as the grace does.
        let half = CARET_BLINK_PERIOD / 2.0;
        assert!(caret_is_visible(CARET_SOLID_FOR + 0.01));
        assert!(!caret_is_visible(CARET_SOLID_FOR + half + 0.01), "never went dark");
        assert!(caret_is_visible(CARET_SOLID_FOR + CARET_BLINK_PERIOD + 0.01), "never came back");
    }

    /// It has to keep blinking, not settle. A phase built from a saturating or clamped
    /// value looks right for one cycle and then stops, which reads as the caret vanishing.
    #[test]
    fn it_keeps_blinking_a_minute_in() {
        let mut seen_on = false;
        let mut seen_off = false;
        let mut t = 60.0f32;
        while t < 60.0 + CARET_BLINK_PERIOD * 2.0 {
            if caret_is_visible(t) { seen_on = true } else { seen_off = true }
            t += CARET_BLINK_PERIOD / 16.0;
        }
        assert!(seen_on && seen_off, "the blink stopped: on={seen_on} off={seen_off}");
    }
}

#[cfg(test)]
mod card_overflow_tests {
    use super::*;
    use vellum_doc::{NewItem, Placement, StyledText};
    // The three fixtures every draw test needs live in `mod tests`, which is a *sibling*
    // rather than a parent — so `use super::*` cannot reach them and they are `pub(super)`
    // for this import alone.
    use super::tests::{camera_at, context, painter, projection_with};

    /// The Amazon shape: a title, **no blurb**, and a 400-character tracking URL.
    ///
    /// Every span in a card's body has to be clipped, not just the description. The
    /// description was the only one that was, and both other paths overflow the same way —
    /// an imported Miro card routinely carries the raw address as its *title*, and the
    /// no-blurb branch appended the whole URL with no clip at all. A URL has no spaces, so
    /// it cannot wrap at a word boundary: it pours out through the bottom of the card and
    /// down the board, which is exactly what the user photographed twice.
    #[test]
    fn no_card_span_can_outrun_its_block() {
        let url = format!("https://www.amazon.com/dp/B0EXAMPLE1?{}", "ref=sr_1_12&crid=26&".repeat(30));
        assert!(url.len() > 400, "the fixture stopped being long");

        for (title, description) in [
            (Some("Superbat Precision Supports Monitor Surveillance".to_owned()), None),
            (Some(url.clone()), None),
            (None, Some(url.clone())),
            (Some(url.clone()), Some(url.clone())),
        ] {
            let kind = ItemKind::LinkPreview {
                title,
                url: Some(url.clone()),
                description,
                thumbnail: None,
                provider: Some("Amazon".to_owned()),
                favicon: None,
                mode: vellum_doc::CardMode::Card,
            };
            const BUDGET: usize = 120;
            let text = card_text(&kind, BlockKey::PRIMARY, BUDGET);
            let drawn = text.to_plain().chars().count();
            // One newline joins the title to the blurb, so the budget may be exceeded by
            // exactly that separator and no more.
            assert!(
                drawn <= BUDGET + 1,
                "a card drew {drawn} characters into a {BUDGET}-character block"
            );
        }
    }

    // ── The Agent Canvas ─────────────────────────────────────────────────────
    //
    // **What these tests cannot see, said once here rather than implied.** `DrawList` exposes
    // no quad reader — feedback 32 records the same limitation for the selection ring — so
    // nothing below asserts a *colour* or a *position on screen*. What they can assert is
    // counts, layout agreement, ordering and purity, and where a colour genuinely matters the
    // assertion is made against the palette the colour is taken from instead. The honest check
    // of what an agent node looks like is a `--screenshot`.

    use vellum_agent::transcript::{AgentRef, Choice, RequestId, TurnId, TurnOutcome};

    fn agent_view(events: Vec<TranscriptEvent>) -> AgentView {
        AgentView {
            status: Status::Running,
            detail: "ran 3 tools".into(),
            subtitle: "Claude · subscription".into(),
            mode: DisplayMode::Clean,
            // Shared in the view, owned by the caller here — the tests are about what is
            // drawn, not about who holds the events.
            events: events.into_iter().map(std::sync::Arc::new).collect(),
            truncated: false,
            draft: String::new(),
            caret: None,
            voice: None,
        }
    }

    fn said(text: &str) -> TranscriptEvent {
        TranscriptEvent::Text { text: text.to_owned() }
    }

    fn node_layout() -> AgentLayout {
        crate::agent::layout(crate::agent::DEFAULT_SIZE.0, crate::agent::DEFAULT_SIZE.1)
    }

    fn node_font() -> f64 {
        node_font_size(crate::agent::DEFAULT_SIZE.0)
    }

    /// The two status colours this file spells out are the chrome's, to the byte.
    ///
    /// This is the join `inspect.rs`'s `THEME_BORDER` did not have and was burned by: a
    /// constant hand-copied out of another crate goes stale where nothing on screen shows it.
    /// `Theme::with_accent` already carries the same arrangement for the accent swatches, and
    /// this is the same test for the same reason.
    #[test]
    fn the_status_colours_are_the_chromes_own_warning_and_danger() {
        let chrome = vellum_ui::theme::Palette::LIGHT;
        for (name, canvas, ui) in [
            ("needs you", STATUS_NEEDS_YOU, chrome.warning),
            ("failed", STATUS_FAILED, chrome.danger),
        ] {
            assert_eq!(
                canvas.pack(),
                [ui.r(), ui.g(), ui.b(), ui.a()],
                "the `{name}` dot disagrees with the chrome's own token"
            );
        }
    }

    /// ⚠ **Typing into a prompt row drew nothing at all**, and every unit test passed
    /// throughout: `type_prompt_key` wrote into its own buffer, the runtime's stored draft was
    /// written only when the session *ended*, and `AgentView::draft` came from the stored one
    /// — so the row said *"Ask this agent to do something"* for the whole of the typing. This
    /// is the painter's half of the seam that closes it.
    ///
    /// Three things are asserted together because each is wrong on its own:
    ///
    /// - The run holds the buffer's string **exactly**. `NodePaint::text` ellipsises to what
    ///   the box was measured for, which is right for a transcript and puts the caret at the
    ///   wrong character in a field — `layout.caret` indexes the string it was given.
    /// - It is pushed **even when empty**, so an empty prompt with a caret in it still has a
    ///   block to be measured from. That is feedback 25, the caret that would not appear in an
    ///   empty sticky, arriving in a fourth place.
    /// - The **placeholder is suppressed**, or the caret sits inside a sentence the user is
    ///   not typing.
    #[test]
    fn a_prompt_row_with_the_keyboard_draws_the_buffer_and_not_the_invitation() {
        let laid = node_layout();
        let font = node_font();
        let caret = crate::agent_view::PromptCaret { cursor: 3, anchor: 3, idle_for: 0.0 };

        // No caret: the stored draft, or the invitation when there is none.
        let resting = agent_view(Vec::new());
        let paint = agent_paint(&resting, &laid, font);
        assert!(
            paint.runs.iter().any(|run| run.text.contains("Ask this agent")),
            "an untouched prompt row lost its invitation"
        );
        assert!(paint.prompt_slot().is_none(), "a row nobody is typing into claimed the caret");

        // With the keyboard in it, and a string long enough that clipping would show.
        let typed = "check the torque figures against the workshop manual and report back";
        let live = AgentView {
            draft: typed.to_owned(),
            caret: Some(caret),
            ..agent_view(Vec::new())
        };
        let paint = agent_paint(&live, &laid, font);
        let slot = paint.prompt_slot().expect("the prompt row claimed no slot");
        let index = usize::from(slot - CELL_SLOT_BASE);
        let run = paint.run(index).expect("the prompt slot names no run");
        assert_eq!(run.text, typed, "the shaped string is not the buffer's own");
        assert!(
            !paint.runs.iter().any(|run| run.text.contains("Ask this agent")),
            "the invitation was drawn under the caret"
        );

        // Empty, with the caret in it. `NodePaint::text` refuses an empty string; the field
        // path must not, or there is no block and nothing for the caret to measure against.
        let blank = AgentView { draft: String::new(), caret: Some(caret), ..agent_view(Vec::new()) };
        let paint = agent_paint(&blank, &laid, font);
        let slot = paint.prompt_slot().expect("an empty prompt row with a caret claimed no slot");
        let run = paint
            .run(usize::from(slot - CELL_SLOT_BASE))
            .expect("the caret has nothing to be drawn against");
        assert!(run.text.is_empty());
        assert!(
            !paint.runs.iter().any(|run| run.text.contains("Ask this agent")),
            "an empty row with the keyboard in it still offered the invitation"
        );
    }

    /// Arriving in the prompt row changes what is drawn without changing a byte of text, so
    /// the cache has to see it. Without this the row keeps the placeholder it was built with
    /// and the caret is drawn over the word "Ask".
    #[test]
    fn the_node_cache_notices_the_keyboard_arriving_in_a_prompt_row() {
        let kind = ItemKind::Agent { model: String::new(), label: StyledText::plain("Planner") };
        let resting = agent_view(Vec::new());
        let live = AgentView {
            caret: Some(crate::agent_view::PromptCaret { cursor: 0, anchor: 0, idle_for: 0.0 }),
            ..agent_view(Vec::new())
        };
        assert_ne!(
            node_signature(&kind, Some(&resting)),
            node_signature(&kind, Some(&live)),
            "a node whose prompt row took the keyboard signed the same as one that had not"
        );
    }

    /// The rows the press path resolves against are the rows that were **drawn**, which is not
    /// the same list as the rows the tree holds: `visible_rows()` bounds them and one is held
    /// back for the "n more" line. A press path that re-derived any of that would expand the
    /// wrong directory — which is academic next to what it actually did, which was nothing at
    /// all, because `filetree::row_at` had no caller.
    #[test]
    fn a_file_trees_drawn_rows_are_the_ones_a_press_resolves_against() {
        use vellum_agent::filetree::{Entry, Row, View as TreeRows};

        let entry = |name: &str, is_dir: bool| Entry {
            name: name.to_owned(),
            relative: name.to_owned(),
            path: std::path::PathBuf::from("/tmp").join(name),
            is_dir,
            len: 0,
            ignored: false,
            is_symlink: false,
        };
        // Far more rows than fit, so the bound is exercised rather than assumed.
        let rows: Vec<Row> = (0..200)
            .map(|n| Row { entry: entry(&format!("item-{n}"), n % 2 == 0), depth: 0, expanded: false })
            .collect();
        let view = TreeRows { rows, truncated: false };

        let laid = crate::filetree::layout(
            crate::filetree::DEFAULT_SIZE.0,
            crate::filetree::DEFAULT_SIZE.1,
        );
        let model = vellum_agent::FileTreeModel::default();
        let paint =
            tree_paint(&model, Some(&view), &laid, node_font_size(crate::filetree::DEFAULT_SIZE.0), 0);

        assert!(!paint.tree_rows.is_empty(), "a drawn tree recorded no pressable rows");
        assert!(
            paint.tree_rows.len() < laid.visible_rows(),
            "every visible row was drawn, leaving none for the \"n more\" line"
        );
        // Every recorded row answers at its own centre, and answers with itself.
        for row in &paint.tree_rows {
            let (cx, cy) = (row.rect.x + row.rect.width / 2.0, row.rect.y + row.rect.height / 2.0);
            let (found, _) = paint.tree_row_at(cx, cy).expect("a drawn row was not pressable");
            assert_eq!(found.relative, row.relative, "a press landed on the wrong row");
        }
        // A directory's triangle is inside its own row and is reported as the triangle; a file
        // has none, and a press on a file's row must not claim to be on one.
        let directory = paint.tree_rows.iter().find(|row| row.is_dir).expect("no directory drawn");
        let twisty = directory.twisty.expect("a directory drew no disclosure triangle");
        let (found, on_twisty) = paint
            .tree_row_at(twisty.x + twisty.width / 2.0, twisty.y + twisty.height / 2.0)
            .expect("a disclosure triangle was not pressable");
        assert_eq!(found.relative, directory.relative);
        assert!(on_twisty, "a press on the triangle did not report as one");

        let file = paint.tree_rows.iter().find(|row| !row.is_dir).expect("no file drawn");
        assert!(file.twisty.is_none(), "a file drew a disclosure triangle");
        let (_, on_twisty) = paint
            .tree_row_at(file.rect.x + 2.0, file.rect.y + file.rect.height / 2.0)
            .expect("a file row was not pressable");
        assert!(!on_twisty);

        // Outside the list entirely — the header, and past the last drawn row.
        assert!(paint.tree_row_at(laid.list.x + 1.0, laid.header.y + 1.0).is_none());
        assert!(
            paint
                .tree_row_at(laid.list.x + 1.0, laid.list.y + laid.list.height + 10.0)
                .is_none()
        );
    }

    /// The four states have to be tellable apart under **every** accent Preferences offers,
    /// and one of the three makes two of them the same colour: `Accent::Red` *is* the danger
    /// coral. So the distinction is carried by the halo — the mark, not the palette — and this
    /// asserts both halves: the colours part where they can, and the halo parts them where the
    /// colour cannot.
    #[test]
    fn the_four_statuses_stay_distinguishable_under_every_accent() {
        let states =
            [Status::Idle, Status::Running, Status::WaitingForPermission, Status::Error];
        for accent in vellum_ui::Accent::ALL {
            let theme = Theme::LIGHT.with_accent(accent);
            let colours: Vec<[u8; 4]> =
                states.iter().map(|s| status_colour(*s, &theme).pack()).collect();
            // Idle and needs-you are never the accent, so they always part from everything.
            assert_ne!(colours[0], colours[1], "{accent:?}: idle and working are one colour");
            assert_ne!(colours[0], colours[3], "{accent:?}: idle and failed are one colour");
            assert_ne!(colours[2], colours[1], "{accent:?}: needs-you and working are one colour");
            assert_ne!(colours[2], colours[3], "{accent:?}: needs-you and failed are one colour");
        }
        // …and the pair that *can* collide is parted by the halo instead. `needs_attention` is
        // the single definition of "wants a person" and the halo is drawn from it.
        assert!(!Status::Running.needs_attention());
        assert!(Status::WaitingForPermission.needs_attention() && Status::Error.needs_attention());
    }

    /// The halo is one extra quad and nothing else, so a working node costs exactly what an
    /// idle one does.
    ///
    /// A count rather than a colour, because `DrawList` has no quad reader — see the note at
    /// the head of this section. A/B'd against the same call with `attention: false`.
    #[test]
    fn only_a_node_that_wants_a_person_draws_a_halo() {
        let camera = camera_at(1.0);
        let count = |attention: bool| {
            let mut list = DrawList::new();
            let board = list.view(vellum_render::View::board(&camera));
            list.use_view(board);
            push_status_dot(
                &mut list,
                ([0.0, 0.0], [10.0, 10.0]),
                Theme::LIGHT.accent,
                attention,
                0.0,
                1.0,
            );
            list.stats().quads
        };
        assert_eq!(count(false), 1, "an unblocked node drew more than its dot");
        assert_eq!(count(true), 2, "a blocked node drew no halo");
    }

    /// The property a live transcript rests on: what just happened is what you can see.
    ///
    /// Laying entries out oldest-first and cutting the overflow is the obvious version and
    /// would pass every "the node draws some text" assertion while losing the only line anyone
    /// is watching. So this asserts the *newest* survived, the oldest did not, and that the cut
    /// is stated on the node rather than left to look like the whole story.
    #[test]
    fn a_transcript_keeps_its_newest_entry_and_says_what_it_cut() {
        let events: Vec<TranscriptEvent> =
            (0..40).map(|n| said(&format!("entry number {n}"))).collect();
        let paint = agent_paint(&agent_view(events), &node_layout(), node_font());
        let drawn: Vec<&str> = paint.runs.iter().map(|run| run.text.as_str()).collect();

        assert!(
            drawn.iter().any(|text| text.contains("entry number 39")),
            "the newest entry was not drawn: {drawn:?}"
        );
        assert!(
            !drawn.iter().any(|text| text.contains("entry number 0")),
            "a 400-unit node drew all forty entries"
        );
        assert!(
            drawn.iter().any(|text| text.contains("some output is not shown")),
            "the node cut its history and did not say so: {drawn:?}"
        );
    }

    /// A short transcript is the whole story and must not claim otherwise. The elision line is
    /// what a reader trusts; one that is always there says nothing.
    #[test]
    fn a_transcript_that_fits_claims_nothing_was_cut() {
        let paint = agent_paint(&agent_view(vec![said("done")]), &node_layout(), node_font());
        assert!(
            !paint.runs.iter().any(|run| run.text.contains("some output")),
            "a two-line transcript said it had been cut"
        );
        assert!(paint.runs.iter().any(|run| run.text.contains("done")));
    }

    /// Nothing a node draws may escape the node.
    ///
    /// A piece outside its own item is painted where nothing can be clicked and, on a frame,
    /// over whatever is beside it — the badge-on-a-short-card failure (feedback 25) in a new
    /// place. Every event kind is in the fixture on purpose: the plate arithmetic differs per
    /// kind, and a check that only covered prose would have passed on the one that does not.
    #[test]
    fn no_piece_of_an_agent_node_escapes_its_own_box() {
        let events = vec![
            TranscriptEvent::TurnStarted { turn: TurnId(1), prompt: "go".into() },
            TranscriptEvent::ToolCall {
                id: vellum_agent::transcript::ToolCallId("t".into()),
                name: "bash".into(),
                input: "ls".into(),
            },
            said("a long answer that will certainly have to wrap more than once, twice even"),
            TranscriptEvent::Message {
                from: AgentRef::new("1@2", "Planner"),
                text: "take this".into(),
            },
            TranscriptEvent::Error { message: "claude: command not found".into() },
            TranscriptEvent::PermissionRequest {
                id: RequestId("r".into()),
                summary: "write to src/main.rs".into(),
                detail: "the file is tracked".into(),
            },
            TranscriptEvent::Image { blob: "abc".into(), caption: Some("a chart".into()) },
            TranscriptEvent::Options {
                prompt: "which direction".into(),
                choices: vec![
                    Choice::new("a", "Cards").with_body("dense").with_image("h1"),
                    Choice::new("b", "List").with_body("plain"),
                    Choice::new("c", "Grid"),
                ],
                chosen: Some("b".into()),
            },
            TranscriptEvent::TurnEnded { turn: TurnId(1), outcome: TurnOutcome::Completed },
        ];

        for (w, h) in [
            crate::agent::DEFAULT_SIZE,
            (crate::agent::MIN_SIZE.0, crate::agent::MIN_SIZE.1),
            (1400.0, 260.0),
            (240.0, 1400.0),
            (520.0, 200.0),
        ] {
            let laid = crate::agent::layout(w, h);
            let paint = agent_paint(&agent_view(events.clone()), &laid, node_font_size(w));
            let inside = |name: &str, rect: NodeRect| {
                assert!(
                    rect.x >= -0.001
                        && rect.y >= -0.001
                        && rect.x + rect.width <= w + 0.001
                        && rect.y + rect.height <= h + 0.001,
                    "a {name} escaped a {w}x{h} node: {rect:?}"
                );
            };
            for run in &paint.runs {
                inside("run", run.rect);
            }
            for plate in &paint.plates {
                inside("plate", plate.rect);
            }
            for image in &paint.images {
                inside("picture", image.rect);
            }
            for card in &paint.options {
                inside("option card", card.rect);
            }
        }
    }

    /// An option card is pressable exactly where it is painted.
    ///
    /// The `row_rect`/`row_at` shape: the press path asks this same function, so the assertion
    /// is that the two directions agree. Aiming at "the second card" instead would pass on a
    /// build whose `option_at` answered for the wrong one, which is the only mistake that
    /// matters here — the three cards differ by an id and nothing else on screen.
    #[test]
    fn an_option_card_is_pressable_where_it_was_painted() {
        let events = vec![TranscriptEvent::Options {
            prompt: "pick one".into(),
            choices: vec![
                Choice::new("first", "Cards"),
                Choice::new("second", "List"),
                Choice::new("third", "Grid"),
            ],
            chosen: None,
        }];
        let paint = agent_paint(&agent_view(events), &node_layout(), node_font());
        assert_eq!(paint.options().len(), 3, "three choices did not make three cards");

        for card in paint.options() {
            let (cx, cy) = (
                card.rect.x + card.rect.width / 2.0,
                card.rect.y + card.rect.height / 2.0,
            );
            let hit = paint.option_at(cx, cy).expect("a card was not pressable at its centre");
            assert_eq!(hit.choice, card.choice, "a press landed on the wrong card");
            // The question the card answers travels with it, so a node holding two
            // unanswered sets resolves the press to the one that was clicked.
            assert_eq!(hit.prompt, card.prompt, "a card lost which question it belonged to");
            assert!(!hit.prompt.is_empty(), "a card carried no question");
        }
        // The gap between two cards belongs to neither.
        let (a, b) = (&paint.options()[0], &paint.options()[1]);
        let between = (a.rect.x + a.rect.width + b.rect.x) / 2.0;
        assert!(
            paint.option_at(between, a.rect.y + 1.0).is_none(),
            "the gutter between two cards answered as a card"
        );
        assert!(paint.option_at(-10.0, -10.0).is_none());
    }

    /// A chosen option is marked, and only the chosen one.
    #[test]
    fn the_option_the_user_picked_is_the_one_marked() {
        let events = vec![TranscriptEvent::Options {
            prompt: "pick one".into(),
            choices: vec![Choice::new("a", "Cards"), Choice::new("b", "List")],
            chosen: Some("b".into()),
        }];
        let paint = agent_paint(&agent_view(events), &node_layout(), node_font());
        let marked: Vec<&str> = paint
            .options()
            .iter()
            .filter(|card| card.chosen)
            .map(|card| card.choice.as_str())
            .collect();
        assert_eq!(marked, ["b"], "the wrong card was marked, or more than one");
        assert!(
            paint.plates.iter().any(|plate| plate.tone == PlateTone::Chosen),
            "the chosen card drew no ring"
        );
    }

    /// **The bug a generation-keyed cache would have.**
    ///
    /// A transcript's words are not in the document, so nothing the projection knows about
    /// moves when an agent speaks. A run therefore carries a stamp derived from its own
    /// content: same words, same stamp — so a still node re-uses its layouts; new words, new
    /// stamp — so the text cache re-shapes. Without this the node draws the previous sentence
    /// forever, at the same slot, and every assertion about *counts* stays green.
    #[test]
    fn a_runs_stamp_follows_its_words_and_not_the_document() {
        let laid = node_layout();
        let font = node_font();
        let first = agent_paint(&agent_view(vec![said("thinking about it")]), &laid, font);
        let again = agent_paint(&agent_view(vec![said("thinking about it")]), &laid, font);
        let changed = agent_paint(&agent_view(vec![said("thought about it")]), &laid, font);

        let stamps = |paint: &NodePaint| -> Vec<u64> {
            paint.runs.iter().map(|run| run.stamp).collect()
        };
        assert_eq!(stamps(&first), stamps(&again), "an unchanged transcript re-shaped");
        assert_ne!(
            stamps(&first),
            stamps(&changed),
            "a changed transcript kept the previous sentence's layout"
        );
    }

    /// A node too small for its own header draws a badge, not a broken layout.
    ///
    /// `AgentLayout::hit` already refuses to offer a control at this size, and `agent_paint`
    /// has to agree: a transcript laid out into rectangles nobody can read is quads nobody can
    /// use, and a control drawn where `hit` answers `None` is a button that does not respond.
    #[test]
    fn a_compact_node_lays_out_a_badge_rather_than_a_transcript() {
        let laid = crate::agent::layout(60.0, 40.0);
        assert!(laid.too_small);
        let paint = agent_paint(&agent_view(vec![said("hello"), said("world")]), &laid, 13.0);
        assert!(paint.runs.is_empty(), "a compact node laid out a transcript");
        assert!(paint.plates.is_empty() && paint.options.is_empty());
    }

    /// The estimate that decides how tall a run is must not be able to abort the process.
    ///
    /// Transcript text is the least controlled string in the application and
    /// `[profile.release]` sets `panic = "abort"`, so `strip_site_affix`'s two aborts
    /// (feedback 30) are the standing precedent: **characters, never bytes.** The CJK case is
    /// also the one the advance estimate is *wrong* about, which is exactly why the clip
    /// exists — so this asserts the clip holds as well as that nothing panics.
    #[test]
    fn a_run_survives_any_text_and_is_clipped_to_its_own_box() {
        let cases = [
            String::new(),
            "   ".into(),
            "夕".repeat(4_000),
            "🙂".repeat(2_000),
            "café".repeat(1_000),
            format!("line one\nline two\n{}", "x".repeat(5_000)),
        ];
        for text in cases {
            let mut paint = NodePaint::default();
            paint.text(NodeRect::new(0.0, 0.0, 200.0, 40.0), &text, 13.0, Tone::Primary, Align::Left);
            for run in &paint.runs {
                // Two lines at 13 units in a 200-unit box is about 60 characters; the ellipsis
                // is the sixty-first. A generous bound, because the assertion is that the clip
                // *happened at all* — an unclipped 4,000-character run is the text that pours
                // out through the bottom of the node.
                assert!(
                    run.text.chars().count() <= 128,
                    "a run drew {} characters into a two-line box",
                    run.text.chars().count()
                );
            }
        }
    }

    /// A file tree draws what is on screen and never what exists.
    ///
    /// The sharpest case the whole canvas is built for: a `target/` directory holds forty
    /// thousand entries. `TreeLayout::visible_rows` is the bound and this asserts the painter
    /// honours it — and that the node *says* there is more, since a list that stops silently is
    /// one the reader believes is complete.
    #[test]
    fn a_file_tree_draws_only_the_rows_that_fit_and_counts_the_rest() {
        use vellum_agent::filetree::{Entry, Row};
        let rows: Vec<Row> = (0..4_000)
            .map(|n| Row {
                entry: Entry {
                    name: format!("file-{n}.rs"),
                    relative: format!("src/file-{n}.rs"),
                    path: std::path::PathBuf::from(format!("/p/src/file-{n}.rs")),
                    is_dir: n % 10 == 0,
                    len: 12,
                    ignored: false,
                    is_symlink: false,
                },
                depth: usize::from(n % 10 != 0),
                expanded: n % 10 == 0,
            })
            .collect();
        let view = TreeView { rows, truncated: true };
        let laid = crate::filetree::layout(
            crate::filetree::DEFAULT_SIZE.0,
            crate::filetree::DEFAULT_SIZE.1,
        );
        let paint = tree_paint(
            &vellum_agent::FileTreeModel::default(),
            Some(&view),
            &laid,
            node_font_size(crate::filetree::DEFAULT_SIZE.0),
            0,
        );

        let fits = laid.visible_rows();
        assert!(fits > 0 && fits < 100, "a 420-unit tree claimed {fits} rows");
        // One run for the header, one per drawn row, one for the count.
        assert!(
            paint.runs() <= fits + 1,
            "a 4,000-row tree shaped {} runs into {fits} rows",
            paint.runs()
        );
        assert!(
            paint.runs.iter().any(|run| run.text.contains("more")),
            "a truncated tree did not say so"
        );
    }

    /// ⚠ **Everything below the fold used to be unreachable.** The list drew the first
    /// `visible_rows()` and stopped, with no gesture to reach the rest — so a `src` directory
    /// with forty files showed the first dozen and named the remainder in a line the user
    /// could do nothing with.
    ///
    /// Two properties, and each rules out a build the other would pass: a scrolled list must
    /// **start at a different row**, and the press path must resolve a click on a scrolled row
    /// to *that* row — which it does by construction here, because both read the rectangles
    /// this function records.
    #[test]
    fn a_scrolled_tree_shows_later_rows_and_presses_resolve_to_them() {
        use vellum_agent::filetree::{Entry, Row};
        let rows: Vec<Row> = (0..100)
            .map(|n| Row {
                entry: Entry {
                    name: format!("file-{n}.rs"),
                    relative: format!("file-{n}.rs"),
                    path: std::path::PathBuf::from(format!("/p/file-{n}.rs")),
                    is_dir: false,
                    len: 12,
                    ignored: false,
                    is_symlink: false,
                },
                depth: 0,
                expanded: false,
            })
            .collect();
        let view = TreeView { rows, truncated: false };
        let laid = crate::filetree::layout(
            crate::filetree::DEFAULT_SIZE.0,
            crate::filetree::DEFAULT_SIZE.1,
        );
        let font = node_font_size(crate::filetree::DEFAULT_SIZE.0);
        let model = vellum_agent::FileTreeModel::default();

        let top = tree_paint(&model, Some(&view), &laid, font, 0);
        let scrolled = tree_paint(&model, Some(&view), &laid, font, 20);

        let first = |paint: &NodePaint| paint.tree_rows()[0].relative.clone();
        assert_eq!(first(&top), "file-0.rs");
        assert_eq!(first(&scrolled), "file-20.rs", "the list did not move");
        assert_eq!(
            top.tree_rows().len(),
            scrolled.tree_rows().len(),
            "scrolling changed how many rows fit"
        );

        // A press at the first row's centre resolves to the row now drawn there — the whole
        // point of the press path reading painted rectangles rather than re-deriving an index.
        let row = &scrolled.tree_rows()[0];
        let (cx, cy) = (
            row.rect.x + row.rect.width / 2.0,
            row.rect.y + row.rect.height / 2.0,
        );
        let (hit, _) = scrolled.tree_row_at(cx, cy).expect("the first drawn row is pressable");
        assert_eq!(hit.relative, "file-20.rs");

        // Past the end, the offset is clamped rather than emptying the list: a stale offset
        // after directories close must degrade to the last screenful.
        let past = tree_paint(&model, Some(&view), &laid, font, 10_000);
        assert!(!past.tree_rows().is_empty(), "an over-scrolled tree drew nothing");
    }

    /// A tree with nothing read yet says so rather than drawing an empty well.
    #[test]
    fn a_tree_with_no_rows_explains_itself() {
        let laid = crate::filetree::layout(
            crate::filetree::DEFAULT_SIZE.0,
            crate::filetree::DEFAULT_SIZE.1,
        );
        let paint = tree_paint(&vellum_agent::FileTreeModel::default(), None, &laid, 13.0, 0);
        assert!(
            paint.runs.iter().any(|run| run.text.contains("not been read")),
            "an unread tree drew an empty box"
        );
    }

    /// A note with no body says which of the two states it is in — the remedies differ.
    #[test]
    fn a_note_with_no_body_names_the_state_it_is_in() {
        let laid = crate::note::layout(crate::note::DEFAULT_SIZE.0, crate::note::DEFAULT_SIZE.1);
        let font = node_font_size(crate::note::DEFAULT_SIZE.0);

        let unnamed = note_paint(&vellum_agent::NoteModel::default(), None, &laid, font);
        assert!(unnamed.runs.iter().any(|run| run.text.contains("no file yet")));

        let named = vellum_agent::NoteModel { path: ".velm/notes/plan.md".into(), ..Default::default() };
        let unread = note_paint(&named, None, &laid, font);
        assert!(unread.runs.iter().any(|run| run.text.contains("not been read")));
        // The footer still names the file and its scope, whether or not the body arrived.
        assert!(unread.runs.iter().any(|run| run.text.contains("plan.md")));
        assert!(unread.runs.iter().any(|run| run.text.contains("Shared")));

        let read = note_paint(&named, Some("# Plan\n\nShip the thing."), &laid, font);
        assert!(read.runs.iter().any(|run| run.text.contains("Ship the thing")));
    }

    /// A browser node with no engine is a card that says why, never a dead rectangle — and the
    /// two reasons are different because the remedies are.
    #[test]
    fn a_browser_node_with_no_engine_says_which_switch_is_off() {
        let laid =
            crate::browser::layout(crate::browser::DEFAULT_SIZE.0, crate::browser::DEFAULT_SIZE.1);
        let font = node_font_size(crate::browser::DEFAULT_SIZE.0);
        let model = vellum_agent::BrowserModel {
            url: "https://example.com/spec".into(),
            title: "The spec".into(),
            live: false,
        };
        let reason = crate::browser::placeholder_reason(&model, false).map(str::to_owned);
        let paint = browser_paint(&model, reason, &laid, font);
        assert!(paint.runs.iter().any(|run| run.text.contains("The spec")), "no page name");
        assert!(
            paint.runs.iter().any(|run| run.text.contains("Preferences")),
            "a dormant browser node drew no explanation"
        );
        assert!(paint.runs.iter().any(|run| run.text.contains("example.com")), "no address");
    }

    /// The two kinds of agent link have to be tellable apart, and an ordinary connector must
    /// keep drawing exactly as it did — which is what makes this layer free on a board that
    /// does not use it.
    #[test]
    fn the_two_agent_links_are_drawn_differently_and_a_plain_one_is_untouched() {
        let theme = Theme::LIGHT;
        assert!(agent_link_look(crate::agent::LinkKind::Plain, &theme).is_none());

        let (message, message_ink) =
            agent_link_look(crate::agent::LinkKind::Message(crate::agent::Direction::Both), &theme)
                .expect("a message link is drawn differently");
        let (context, context_ink) = agent_link_look(crate::agent::LinkKind::Context, &theme)
            .expect("a context link is drawn differently");
        assert_ne!(message, context, "the two agent links share a cadence");
        assert_ne!(
            message_ink.pack(),
            context_ink.pack(),
            "the two agent links share a colour as well as a cadence"
        );
    }

    /// **The dash cadence is in screen pixels, and this is the assertion that says so.**
    ///
    /// `vellum_connect` derives its pattern from the thickness, so setting the thickness in
    /// device pixels is what makes the cadence screen-constant — the whole reason the link is
    /// drawn that way rather than at the connector's own world thickness. A world-unit cadence
    /// is a solid smear at a fitted 4% and three dashes across the window at 8×, which is the
    /// failure `push_grid` records for the board's own dots.
    ///
    /// Measured as the pattern's **on-screen** period at two zooms three orders apart.
    #[test]
    fn an_agent_links_dash_cadence_is_the_same_size_on_screen_at_any_zoom() {
        let period_on_screen = |zoom: f64| {
            let thickness = AGENT_LINK_WIDTH / zoom;
            let pattern = vellum_connect::LineStyle::Dashed
                .dash_pattern(thickness)
                .expect("a dashed line has a pattern");
            (pattern.on + pattern.off) * zoom
        };
        let near = period_on_screen(8.0);
        let far = period_on_screen(0.04);
        assert!(
            (near - far).abs() < 1e-6,
            "the cadence is {near} device px at 8x and {far} at 4%"
        );
    }

    /// Past the cap the cadence is dropped rather than stretched.
    ///
    /// A connector between two agents at opposite ends of a board is mostly off screen at a
    /// working zoom and `vellum_connect` has no viewport to clip against, so an unbounded
    /// cadence is `GUIDE_MAX_DASHES`'s hazard in a new place: a frame that emits a hundred
    /// thousand triangles for a rhythm nobody can see.
    #[test]
    fn a_link_too_long_to_dash_is_drawn_solid_rather_than_stretched() {
        let dashes = |length: f64, zoom: f64| {
            let thickness = AGENT_LINK_WIDTH / zoom;
            let pattern = vellum_connect::LineStyle::Dashed.dash_pattern(thickness).unwrap();
            length / (pattern.on + pattern.off)
        };
        // An ordinary link at a working zoom is nowhere near the cap.
        assert!(dashes(600.0, 1.0) < AGENT_LINK_MAX_DASHES);
        // One across a whole board at 8x is well past it.
        assert!(dashes(100_000.0, 8.0) > AGENT_LINK_MAX_DASHES);
    }

    /// A pulse travels the path, and it travels it the way the message went.
    ///
    /// `LinkPulse::forward` is start → end, which is the direction the routed path was built
    /// in, so a backward message is the same journey walked from the other end. Getting this
    /// wrong draws every reply moving the way the question went, which looks correct until two
    /// agents answer each other.
    #[test]
    fn a_pulse_travels_the_path_in_the_direction_the_message_went() {
        use vellum_connect::{Point, Polyline};
        let line = Polyline::new([
            Point::new(0.0, 0.0),
            Point::new(100.0, 0.0),
            Point::new(100.0, 100.0),
        ]);
        let at = |t: f64| point_along(&line, t).expect("a path with extent has points on it");

        assert_eq!(at(0.0), Point::new(0.0, 0.0));
        assert_eq!(at(1.0), Point::new(100.0, 100.0));
        // Halfway by **arc length**, which on this path is the corner.
        let middle = at(0.5);
        assert!((middle.x - 100.0).abs() < 1e-6 && middle.y.abs() < 1e-6, "{middle:?}");

        // The direction rule itself: the same progress maps to opposite ends.
        let forward = |progress: f64| at(progress);
        let backward = |progress: f64| at(1.0 - progress);
        assert_eq!(forward(0.0), backward(1.0));
        assert_ne!(forward(0.1), backward(0.1));

        // Degenerate paths answer their own only point rather than an arbitrary end — a
        // connector with both ends on one anchor is an ordinary thing to draw.
        let dot = Polyline::new([Point::new(5.0, 5.0)]);
        assert_eq!(point_along(&dot, 0.5), Some(Point::new(5.0, 5.0)));
        assert_eq!(point_along(&Polyline::default(), 0.5), None);
    }

    /// The pulse is two quads and nothing more, and it draws nothing on an idle board.
    ///
    /// The second half is the promise `docs/07` §0 rule 2 makes: *"a board with no agent nodes
    /// must be frame-for-frame the same cost as before this layer existed"*. A count is all a
    /// `DrawList` can be asked, and it is the right question here — an animation that cost a
    /// pass would show up as quads on a frame where nothing is happening.
    #[test]
    fn a_pulse_costs_two_quads_and_an_idle_board_costs_none() {
        use vellum_connect::{Point, Polyline};
        let camera = camera_at(1.0);
        let projection = projection_with([]);
        let ctx = context(&camera, &projection);
        let line = Polyline::new([Point::new(0.0, 0.0), Point::new(200.0, 0.0)]);

        let mut list = DrawList::new();
        let board = list.view(vellum_render::View::board(&camera));
        list.use_view(board);
        push_link_pulse(
            &mut list,
            &ctx,
            &line,
            vellum_agent::LinkPulse { forward: true, progress: 0.5 },
            Theme::LIGHT.accent,
        );
        assert_eq!(list.stats().quads, 2, "the pulse is a dot and its tail");

        // Nothing in flight is nothing drawn, and it is a length check rather than a scan.
        assert!(!ctx.agents.any_in_flight());
        assert!(ctx.agents.is_empty());
        assert!(ctx.agents.pulse(1).is_none());
    }

    /// A node's text size follows its width, so a node dragged out four times carries text
    /// legible from where it was dragged to — and a node made *narrower* keeps its type rather
    /// than shrinking it until nothing is readable at any zoom.
    #[test]
    fn a_nodes_type_grows_with_the_node_and_never_shrinks_below_the_body_size() {
        assert!((node_font_size(NODE_REFERENCE_WIDTH) - NODE_FONT_SIZE).abs() < 1e-9);
        assert!(node_font_size(NODE_REFERENCE_WIDTH * 2.0) > NODE_FONT_SIZE);
        assert!((node_font_size(40.0) - NODE_FONT_SIZE).abs() < 1e-9, "a narrow node shrank its type");
        // Bounded above, or a node dragged to ten thousand units sets its transcript in
        // headlines and shapes one word per line.
        assert!(node_font_size(1_000_000.0) <= NODE_FONT_SIZE * 6.0 + 1e-9);
    }

    /// The role's box and the subtitle's must not overlap: they are derived from the same
    /// rectangle by two different functions — `node_title_box` and `agent_paint` — and two
    /// derivations of one measurement is how a label ends up drawn over another.
    #[test]
    fn the_role_and_the_line_under_it_do_not_overlap() {
        let font = node_font();
        let laid = node_layout();
        let (_, role_y, _, role_h) = node_title_box(
            &ItemKind::Agent { model: String::new(), label: StyledText::plain("Reviewer") },
            crate::agent::DEFAULT_SIZE,
            font,
        );
        // The **detail** line, not the subtitle: this fixture's agent is working, and the
        // header prefers what an agent is doing to what it runs on. Looking for the provider
        // here would fail on that rule rather than on the geometry this test is about — which
        // is the shape of test that gets "fixed" by weakening the thing it was written for.
        let paint = agent_paint(&agent_view(vec![]), &laid, font);
        let subtitle = paint
            .runs
            .iter()
            .find(|run| run.text.contains("ran 3 tools"))
            .expect("the header drew no line under the role");
        assert!(
            role_y + role_h <= subtitle.rect.y + 0.001,
            "the role's box ends at {} and the subtitle starts at {}",
            role_y + role_h,
            subtitle.rect.y
        );
    }

    /// A node with a session shows what it is *doing*; one without shows what it runs on.
    ///
    /// There is room for one line under the role and the live one is worth more, but a node the
    /// runtime has not reached yet must not be blank — it knows its own configuration from its
    /// token, and `agent::subtitle` is the single derivation the inspector reads too.
    #[test]
    fn the_header_prefers_what_the_agent_is_doing_to_what_it_runs_on() {
        let laid = node_layout();
        let font = node_font();

        let busy = agent_paint(&agent_view(vec![]), &laid, font);
        assert!(busy.runs.iter().any(|run| run.text.contains("ran 3 tools")));

        let fresh = AgentView { detail: String::new(), ..agent_view(vec![]) };
        let quiet = agent_paint(&fresh, &laid, font);
        assert!(
            quiet.runs.iter().any(|run| run.text.contains("subscription")),
            "a node with nothing in flight drew no provider line"
        );
    }

    /// The prompt row is drawn whether or not anything has been typed into it, and the draft is
    /// what is shown when there is one — `AgentView::draft` is held by the runtime precisely so
    /// a half-written instruction survives clicking away, and a row that did not draw it would
    /// make that promise invisible.
    #[test]
    fn the_prompt_row_shows_the_draft_and_invites_one_when_there_is_none() {
        let laid = node_layout();
        let font = node_font();

        let empty = agent_paint(&agent_view(vec![]), &laid, font);
        assert!(empty.runs.iter().any(|run| run.text.contains("Ask this agent")));

        let typed = AgentView { draft: "refactor the parser".into(), ..agent_view(vec![]) };
        let started = agent_paint(&typed, &laid, font);
        assert!(started.runs.iter().any(|run| run.text.contains("refactor the parser")));
        assert!(
            !started.runs.iter().any(|run| run.text.contains("Ask this agent")),
            "the invitation was drawn over the draft"
        );
    }

    /// A message from another agent is attributed, and it is attributed the right way round.
    ///
    /// `docs/07` §6's whole point is that a wired-up board is followable. A message drawn like
    /// the agent's own words makes two nodes read as one conversation with no author, and a
    /// *sent* message drawn like a received one reverses who said what.
    #[test]
    fn inter_agent_messages_say_who_and_which_way() {
        let events = vec![
            TranscriptEvent::Message {
                from: AgentRef::new("1@2", "Planner"),
                text: "take this".into(),
            },
            TranscriptEvent::MessageSent {
                to: AgentRef::new("3@4", "Builder"),
                text: "done".into(),
            },
        ];
        let paint = agent_paint(&agent_view(events), &node_layout(), node_font());
        let texts: Vec<&str> = paint.runs.iter().map(|run| run.text.as_str()).collect();
        assert!(texts.iter().any(|text| text.contains("from Planner")), "{texts:?}");
        assert!(texts.iter().any(|text| text.contains("to Builder")), "{texts:?}");
        // And both are railed rather than run in with the agent's own prose.
        assert!(
            paint.plates.iter().filter(|plate| plate.tone == PlateTone::Rail).count() >= 2,
            "a message from another agent was drawn as the agent's own words"
        );
    }

    /// A failure and a question each get their own wash, and they are different washes: one
    /// reports something that happened and the other is blocking the agent until a person acts.
    #[test]
    fn a_failure_and_a_question_are_not_drawn_the_same_way() {
        let events = vec![
            TranscriptEvent::Error { message: "claude: command not found".into() },
            TranscriptEvent::PermissionRequest {
                id: RequestId("r1".into()),
                summary: "write to src/main.rs".into(),
                detail: String::new(),
            },
        ];
        let paint = agent_paint(&agent_view(events), &node_layout(), node_font());
        assert!(paint.plates.iter().any(|plate| plate.tone == PlateTone::Failed));
        assert!(paint.plates.iter().any(|plate| plate.tone == PlateTone::NeedsYou));
        assert!(
            paint.runs.iter().any(|run| run.tone == Tone::Failed),
            "the error was drawn in the ordinary ink"
        );
        assert!(
            paint.runs.iter().any(|run| run.text.contains("Waiting for your answer")),
            "a blocked agent did not say it was waiting"
        );
        let chips = paint.permissions();
        assert_eq!(chips.len(), 1);
        assert_eq!(chips[0].request, "r1");
        assert!(!chips[0].allow.is_empty() && !chips[0].deny.is_empty());
        assert!(chips[0].allow.x + chips[0].allow.width <= chips[0].deny.x + 0.001);
    }

    /// **A permission chip is drawn where it is pressed, and it is drawn at all.**
    ///
    /// The row shipped measured and unpainted: the rectangles went into `PermissionChips`, the
    /// press path answered them, and nothing put a plate or a word in the band they reserved.
    /// So the body of a blocked node carried a blank strip that allowed on its left half and
    /// denied on its right — and because the arm returns `true`, a click there could not even
    /// select the node.
    ///
    /// Both halves are asserted, because either alone passes on a build with the defect facing
    /// the other way: the words prove something was painted, and comparing the plates against
    /// the *press* rectangles proves it was painted where the click lands. Aiming at "two more
    /// plates exist" would pass on chips drawn a hundred units below the button.
    #[test]
    fn a_permission_asks_with_two_buttons_drawn_where_the_press_path_reads_them() {
        let events = vec![TranscriptEvent::PermissionRequest {
            id: RequestId("r1".into()),
            summary: "write to src/main.rs".into(),
            detail: String::new(),
        }];
        let paint = agent_paint(&agent_view(events), &node_layout(), node_font());
        let chips = paint.permissions().first().expect("no permission row was measured").clone();

        let (allow, deny) = (chips.allow, chips.deny);
        let word_in = |text: &str, x: f64, y: f64, width: f64, height: f64| {
            paint.runs.iter().any(|run| {
                run.text == text
                    && run.rect.x >= x - 0.001
                    && run.rect.x + run.rect.width <= x + width + 0.001
                    && run.rect.y >= y - 0.001
                    && run.rect.y + run.rect.height <= y + height + 0.001
            })
        };
        assert!(
            word_in("Allow", allow.x, allow.y, allow.width, allow.height),
            "the allow chip drew no word inside itself"
        );
        assert!(
            word_in("Deny", deny.x, deny.y, deny.width, deny.height),
            "the deny chip drew no word inside itself"
        );

        // And a plate under each, at exactly the rectangle the press path answers.
        let plate_at = |x: f64, y: f64, width: f64, height: f64| {
            paint.plates.iter().any(|plate| {
                (plate.rect.x - x).abs() < 0.001
                    && (plate.rect.y - y).abs() < 0.001
                    && (plate.rect.width - width).abs() < 0.001
                    && (plate.rect.height - height).abs() < 0.001
            })
        };
        assert!(
            plate_at(allow.x, allow.y, allow.width, allow.height),
            "the allow chip is a hot zone with nothing drawn on it"
        );
        assert!(
            plate_at(deny.x, deny.y, deny.width, deny.height),
            "the deny chip is a hot zone with nothing drawn on it"
        );

        // The question's own wash is **under** both, or a 10% tint lands over the buttons:
        // `push_node_plate` walks the list in order.
        let wash = paint
            .plates
            .iter()
            .position(|plate| plate.tone == PlateTone::NeedsYou)
            .expect("the question lost its wash");
        // Matched on the whole rectangle, not on `x` alone: the prompt row's own well shares a
        // left edge with the body, and finding *that* would make this assertion about a
        // different plate entirely.
        let first_chip = paint
            .plates
            .iter()
            .position(|plate| {
                (plate.rect.x - allow.x).abs() < 0.001
                    && (plate.rect.y - allow.y).abs() < 0.001
                    && (plate.rect.width - allow.width).abs() < 0.001
                    && (plate.rect.height - allow.height).abs() < 0.001
            })
            .expect("the allow chip has no plate");
        assert!(wash < first_chip, "the question's wash was drawn over its own buttons");
    }

    /// Scaffolding is muted and the answer is not, which is the whole difference between Raw
    /// mode and a wall of text. **The painter does not decide what is shown** — that is
    /// `TranscriptEvent::visible_in_clean_mode`, applied one layer up — so this asserts only
    /// how the events it is *given* are drawn.
    #[test]
    fn scaffolding_is_muted_and_the_answer_is_not() {
        let events = vec![
            TranscriptEvent::ToolCall {
                id: vellum_agent::transcript::ToolCallId("t".into()),
                name: "bash".into(),
                input: "ls -la".into(),
            },
            said("here is what I found"),
        ];
        let paint = agent_paint(&agent_view(events), &node_layout(), node_font());
        let tool = paint
            .runs
            .iter()
            .find(|run| run.text.contains("ran bash"))
            .expect("a tool call drew nothing");
        let answer = paint
            .runs
            .iter()
            .find(|run| run.text.contains("here is what I found"))
            .expect("the answer drew nothing");
        assert_eq!(tool.tone, Tone::Muted);
        assert_eq!(answer.tone, Tone::Primary);
        assert!(tool.font_size < answer.font_size, "scaffolding was set at the body size");
    }

    /// A picture goes in the blob store and is drawn from its hash, exactly as a pasted
    /// screenshot is — so the residency budget and the deduplication come for free and nothing
    /// new is cached. The band is reserved whether or not the texture is resident yet, which is
    /// what stops an image arriving mid-session reflowing the words out from under the reader —
    /// the rule `card_layout` records for a favicon.
    #[test]
    fn a_transcript_picture_reserves_its_band_by_hash() {
        let events = vec![TranscriptEvent::Image {
            blob: "b3-deadbeef".into(),
            caption: Some("the failing test".into()),
        }];
        let paint = agent_paint(&agent_view(events), &node_layout(), node_font());
        assert_eq!(paint.images.len(), 1);
        assert_eq!(paint.images[0].blob, "b3-deadbeef");
        assert!(paint.images[0].rect.height > 0.0);
        assert!(paint.runs.iter().any(|run| run.text.contains("the failing test")));
    }

    /// An agent's role and a note's title take the same caret exception a sticky does.
    ///
    /// **This is feedback 25's lesson as a test rather than as a comment.** The exception —
    /// an empty slot holding the cursor must still produce a block, or the caret has no origin
    /// to be drawn against — was taught to `table_cell_block` and `kanban_run_block` and *not*
    /// to the three plain kinds, and the symptom was a caret that only appeared after the
    /// first keystroke. These two are new call sites for the same rule, and a screenshot of a
    /// working sticky says nothing about either of them.
    #[test]
    fn an_empty_role_or_title_holding_the_caret_still_gets_a_block() {
        for kind in [
            ItemKind::Agent { model: String::new(), label: StyledText::default() },
            ItemKind::AgentNote { model: String::new(), title: StyledText::default() },
        ] {
            let tag = kind.tag();
            let projection = projection_with([NewItem::new(
                kind,
                Placement::new(0.0, 0.0, crate::agent::DEFAULT_SIZE.0, crate::agent::DEFAULT_SIZE.1),
            )]);
            let (&id, projected) = projection.iter().next().unwrap();
            let camera = camera_at(1.0);
            let mut painter = painter();

            assert!(
                painter
                    .block(id, projected, BlockKey::PRIMARY, &context(&camera, &projection))
                    .is_none(),
                "an unnamed {tag} nobody is typing into should cost nothing",
            );

            let mut ctx = context(&camera, &projection);
            ctx.editing = Some(TextCursor {
                scene: id,
                slot: BlockKey::PRIMARY,
                idle_for: 0.0,
                cursor: 0,
                anchor: 0,
                text: "",
            });
            assert!(
                painter.block(id, projected, BlockKey::PRIMARY, &ctx).is_some(),
                "the caret in an empty {tag} has nothing to be drawn against",
            );
        }
    }

    /// `agent_paint` is pure: the same view and the same box give the same answer, every time.
    ///
    /// It has to be, because the **press path calls it too** — `kanban_runs`' rule — and a
    /// layout that depended on anything but its arguments would put a click somewhere the paint
    /// was not on whichever of the two calls happened to disagree.
    #[test]
    fn laying_out_a_node_depends_on_nothing_but_its_arguments() {
        let events = vec![
            said("one"),
            TranscriptEvent::Options {
                prompt: "which".into(),
                choices: vec![Choice::new("a", "A"), Choice::new("b", "B")],
                chosen: None,
            },
        ];
        let view = agent_view(events);
        let laid = node_layout();
        let font = node_font();
        let first = agent_paint(&view, &laid, font);
        let second = agent_paint(&view, &laid, font);

        let boxes = |paint: &NodePaint| -> Vec<(String, NodeRect)> {
            paint.runs.iter().map(|run| (run.text.clone(), run.rect)).collect()
        };
        assert_eq!(boxes(&first), boxes(&second));
        let cards = |paint: &NodePaint| -> Vec<NodeRect> {
            paint.options().iter().map(|card| card.rect).collect()
        };
        assert_eq!(cards(&first), cards(&second));
    }
}
