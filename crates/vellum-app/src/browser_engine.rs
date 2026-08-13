//! The live half of a browser node: where the page goes, when it may go there, and the
//! one place a real web engine is allowed to exist.
//!
//! `crate::browser` owns the token, the layout and the two switches. This owns the
//! *engine* — and the reason it is a separate module is that almost none of what follows
//! needs a webview to be right. The arithmetic that decides where a page is put, and the
//! rules that decide whether it is put anywhere at all, are pure functions with tests.
//! Only [`wry_host`] touches a platform, and it is behind a cargo feature that is off.
//!
//! # A webview is not a texture, and this module refuses to pretend otherwise
//!
//! Everything else Velm draws goes through `vellum-render` into one wgpu surface, which is
//! why a sticky zooms, rotates, is occluded by a frame, exports to SVG and can be
//! photographed by `--screenshot`. A `WKWebView` is none of that. It is a **native child
//! view composited by the window server**, on top of the Metal layer, and the consequences
//! are not negotiable:
//!
//! - **It cannot rotate.** A rotated node hides its engine and draws the card.
//! - **It cannot be occluded** by anything Velm paints — a frame, a panel, a dialog. So the
//!   engine is only shown where nothing of ours is in the way, and the app suspends the
//!   whole pool while a dialog or a menu is up.
//! - **It does not export and does not screenshot.** A `--screenshot` of a live browser
//!   node photographs the card underneath it. That is a limitation to be reported, not a
//!   bug to be chased.
//! - **It eats every event over its own rectangle.** `winit` never sees a click or a wheel
//!   that lands on the page. This is why [`crate::browser::layout`] puts the address, the
//!   reload button and the ↗ badge in a chrome bar *outside* the viewport: that bar is the
//!   only part of a live node the board can still manipulate.
//!
//! # Scale is the lie this module is most tempted to tell
//!
//! A page cannot be drawn at 37% the way a sticky can — there is no transform to apply,
//! only a view of some size showing a page at some zoom. Two ways to be wrong:
//!
//! 1. Size the view to the node's on-screen rectangle and leave the page at 100%. The
//!    page's CSS viewport then shrinks as the board zooms out, so the *page re-lays itself
//!    out* — a responsive site flips to its mobile layout halfway through a pinch. The node
//!    shows different content at different zooms, which is worse than showing none.
//! 2. Clip the view to the visible part of the canvas. A clipped frame does not show the
//!    clipped *portion* of the page; it shows the page's own top-left corner in a smaller
//!    box. A node half off the edge of the screen would draw the wrong part of the page at
//!    the wrong size. This is the one failure mode that is actively misleading.
//!
//! The answer to (1) is [`page_zoom_for`]: set the engine's page zoom to `zoom / scale`, so
//! **one world unit is always one CSS pixel** whatever the camera is doing. A 720-unit-wide
//! browser node is a 720px-wide page at every zoom; the page never reflows because the board
//! moved, and it scales exactly as the rest of the board scales. Outside
//! [`MIN_PAGE_ZOOM`]..=[`MAX_PAGE_ZOOM`] that stops being legible, and below the floor the
//! engine hides and the card draws instead — which is the honest answer.
//!
//! The answer to (2) is [`Hidden::PartlyOffCanvas`]: an engine runs only while its whole
//! viewport is inside the canvas rectangle. Panning a live node past the edge hides it and
//! shows the card. **The upgrade that would remove that restriction is a clipping container
//! view** — one `NSView` per window with `clipsToBounds`, the webview built as a child of
//! *that* rather than of the window — and it is deliberately not attempted here: it is
//! unverifiable `objc2` work in a build that cannot be compiled or run from this seat.
//!
//! # Hide, or destroy
//!
//! The difference costs the user something, so it is stated once here rather than decided
//! per call site:
//!
//! - **Hiding is cheap and keeps everything.** The page stays loaded, the scroll position
//!   stays where it was, a half-filled form stays half filled, a video keeps its place. A
//!   node scrolled off screen, zoomed past, covered or suspended is *hidden*.
//! - **Destroying loses all of that** and cannot be undone by scrolling back. So it happens
//!   only when the user has said the page should stop: the node is deleted, its `live` is
//!   cleared, the preference is turned off, or the board is no longer in front.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use vellum_doc::{ItemId, Placement};
use vellum_scene::{Camera, WorldPoint};

use crate::browser::{BrowserLayout, layout};

/// Whether a real engine is compiled into this binary at all.
///
/// **`false` is the shipping answer**, not a stub: `docs/07-agent-canvas.md` §0 rule 3 has
/// browser nodes off by default and `docs/01-architecture.md` §1 rejects a webview in the
/// canvas. The `browser` cargo feature is the deliberate, documented exception, and with it
/// off a node still places, still persists, still draws its address and still opens the page
/// in the user's own browser — it simply says so. See [`NOT_BUILT`].
pub const ENGINE_BUILT: bool = cfg!(feature = "browser");

/// What a node says when both switches are on and the binary has no engine in it.
///
/// A third reason, distinct from the two [`crate::browser::placeholder_reason`] already
/// knows, because the remedy is different again: the other two are settings the user can
/// change and this one is a build they would have to install.
pub const NOT_BUILT: &str =
    "Browser nodes are not built into this build — the page opens in your own browser";

// ---------------------------------------------------------------------------------------
// Geometry
// ---------------------------------------------------------------------------------------

/// A rectangle in **physical device pixels**, measured from the window's top-left.
///
/// Physical, like every other screen coordinate in this codebase — `CLAUDE.md` trap 4 —
/// and it stays physical all the way into `wry`, which takes a `dpi::PhysicalPosition`
/// and does its own conversion from the window's scale factor. Converting to points here
/// and letting the platform convert again is precisely the double-application that makes
/// things drift by exactly the scale factor.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PixelRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl PixelRect {
    pub const fn new(x: f64, y: f64, width: f64, height: f64) -> Self {
        Self { x, y, width, height }
    }

    pub fn is_empty(&self) -> bool {
        self.width <= 0.0 || self.height <= 0.0
    }

    /// Whether this rectangle lies entirely inside `outer`, with a pixel of slack so a
    /// rounding difference at an exact edge does not flicker an engine on and off.
    pub fn inside(&self, outer: Self) -> bool {
        const SLACK: f64 = 1.0;
        self.x >= outer.x - SLACK
            && self.y >= outer.y - SLACK
            && self.x + self.width <= outer.x + outer.width + SLACK
            && self.y + self.height <= outer.y + outer.height + SLACK
    }

    /// Whether the two overlap at all. Touching edges do not count as overlapping —
    /// a toolbar whose right edge is a node's left edge is not covering it.
    pub fn overlaps(&self, other: Self) -> bool {
        self.x < other.x + other.width
            && other.x < self.x + self.width
            && self.y < other.y + other.height
            && other.y < self.y + self.height
    }
}

/// Everything about the window and the frame that is not the node itself.
///
/// Assembled by `app.rs` once per frame and handed to [`place`] for every browser node, so
/// the answer for two nodes cannot be derived from two different views of the same frame.
#[derive(Debug, Clone, Copy)]
pub struct ViewFrame<'a> {
    /// The part of the window the board can be seen in, in physical pixels — exactly what
    /// `App::canvas_pixels` already computes for the render pass.
    pub canvas: PixelRect,
    /// `Window::scale_factor`. Physical pixels per logical point.
    pub scale: f64,
    /// Chrome that floats **over** the canvas and would be covered by a native view.
    ///
    /// The canvas rectangle deliberately does not subtract the tool palette — the board
    /// runs underneath it on purpose — so the palette's own rectangle has to arrive here
    /// or a page would be drawn on top of the toolbar.
    ///
    /// **The context bar is deliberately not in this list.** It floats above the
    /// *selection*, so treating it as keep-out would blank the page the instant its own
    /// node was clicked, which is when the user is most likely to be looking at it. It
    /// overlaps the node's chrome bar rather than the page in the common case.
    pub keep_out: &'a [PixelRect],
    /// Set while a dialog, the command palette, the find bar or a menu is up.
    ///
    /// A native view is composited above everything Velm paints, so a modal surface would
    /// otherwise open *behind* a live page. `Assets::suspend_decoding` is the precedent:
    /// the state is computed fresh each frame rather than latched, so there is no flag for
    /// one path to forget to clear.
    pub suspended: bool,
}

/// Where a node's page should be this frame, or why it is not anywhere.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Placed {
    /// Put the engine here, at this page zoom, and show it.
    Show { frame: PixelRect, page_zoom: f64 },
    /// Keep the engine and its page state; take it off the screen.
    Hide(Hidden),
}

/// Why a page that is allowed to run is not on the screen.
///
/// Every one of these has to answer with words — nothing in this app is inert — and the
/// words differ because the remedies differ. A test below asserts all of them do.
///
/// There is deliberately **no `Failed` variant**: a refusal carries what the platform said,
/// which is a `String` and not one of these, and a second spelling of the same state is the
/// two-sources-of-truth failure this repository keeps paying for. See `Reported`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Hidden {
    /// The node is not in the visible part of the canvas at all.
    OffScreen,
    /// Part of it is. An engine cannot be clipped honestly — see the module header.
    PartlyOffCanvas,
    /// The board is zoomed out far enough that the page would not be legible.
    ZoomedOut,
    /// The node is too small to hold a page, in world units or in device pixels.
    TooSmall,
    /// A rotated node. A native view has no rotation to give.
    Rotated,
    /// A floating piece of Velm's own chrome is over it.
    Covered,
    /// A dialog, a menu or the palette is up, and a native view would cover it.
    Suspended,
}

impl Hidden {
    /// What the card underneath says. Never empty, and never the same sentence for two
    /// different remedies.
    pub const fn reason(self) -> &'static str {
        match self {
            Self::OffScreen => "This page is off screen",
            Self::PartlyOffCanvas => "Bring the whole node on screen to see the page",
            Self::ZoomedOut => "Zoom in to see the page",
            Self::TooSmall => "This node is too small to show a page",
            Self::Rotated => "A page cannot be shown on a rotated node",
            Self::Covered => "Move the node out from under the toolbar to see the page",
            Self::Suspended => "The page is hidden while this menu is open",
        }
    }

    /// Whether the reason is worth putting on the card.
    ///
    /// A node nobody can see does not need to explain itself, and a node under a menu is
    /// about to be visible again — writing on either would make the board flicker with
    /// sentences that answer a question the user is not asking.
    pub const fn worth_saying(self) -> bool {
        !matches!(self, Self::OffScreen | Self::Suspended)
    }
}

/// The lowest page zoom worth rendering at.
///
/// Below this a page is sub-pixel mush rather than a small page: 12px body text at 0.25
/// is three device pixels tall. `zoom` here is the camera's, which is **physical pixels
/// per world unit**, so on a Retina display this floor is reached at a *displayed* 50%
/// and on a 1× display at 25%.
pub const MIN_PAGE_ZOOM: f64 = 0.25;

/// The highest, clamped rather than refused.
///
/// Past this the frame keeps growing and the page zoom does not, so the CSS viewport gets
/// wider and the page reflows — the same thing a browser does at 300%. That is a legible
/// page at a correct scale, so it degrades rather than hides.
pub const MAX_PAGE_ZOOM: f64 = 3.0;

/// The smallest frame worth handing an engine, in physical pixels.
///
/// A node can be small in world units *and* the board zoomed in, so this is not implied by
/// [`MIN_PAGE_ZOOM`] and is checked separately.
const MIN_FRAME_PIXELS: f64 = 64.0;

/// The page zoom for a camera zoom and a display scale.
///
/// `zoom / scale`, and the whole point is what that makes constant: the frame is
/// `world × zoom` physical pixels, i.e. `world × zoom / scale` points, and a page at zoom
/// `z/s` fills a point with `s/z` CSS pixels — so the CSS viewport is `world` CSS pixels
/// at **every** camera zoom. One world unit, one CSS pixel, always. That is what stops the
/// page re-laying itself out every time the board is zoomed.
pub fn page_zoom_for(zoom: f64, scale: f64) -> f64 {
    if !(zoom.is_finite() && scale.is_finite()) || scale <= 0.0 {
        return 1.0;
    }
    zoom / scale
}

/// Where this node's page goes this frame, or why it does not.
///
/// Pure: a camera, a placement and a description of the frame. No window, no engine, no
/// GPU — which is the point, because this arithmetic is the part that has to be right and
/// a webview is the part that cannot be tested from here.
pub fn place(camera: &Camera, placement: &Placement, view: &ViewFrame<'_>) -> Placed {
    if view.suspended {
        return Placed::Hide(Hidden::Suspended);
    }
    // A native view has no rotation. Checked before any arithmetic, because the axis-aligned
    // conversion below is only exact for an unrotated node.
    if placement.rotation != 0.0 {
        return Placed::Hide(Hidden::Rotated);
    }

    let (width, height) = placement.scaled_size();
    let laid: BrowserLayout = layout(width, height);
    if laid.too_small || laid.viewport.is_empty() {
        return Placed::Hide(Hidden::TooSmall);
    }

    let frame = viewport_pixels(camera, placement, &laid);
    if frame.is_empty() {
        return Placed::Hide(Hidden::TooSmall);
    }
    if !frame.overlaps(view.canvas) {
        return Placed::Hide(Hidden::OffScreen);
    }

    let page_zoom = page_zoom_for(camera.zoom(), view.scale);
    if page_zoom < MIN_PAGE_ZOOM {
        return Placed::Hide(Hidden::ZoomedOut);
    }
    if frame.width < MIN_FRAME_PIXELS || frame.height < MIN_FRAME_PIXELS {
        return Placed::Hide(Hidden::TooSmall);
    }
    if !frame.inside(view.canvas) {
        return Placed::Hide(Hidden::PartlyOffCanvas);
    }
    if view.keep_out.iter().any(|rect| frame.overlaps(*rect)) {
        return Placed::Hide(Hidden::Covered);
    }

    Placed::Show { frame, page_zoom: page_zoom.min(MAX_PAGE_ZOOM) }
}

/// The node's page area, in physical window pixels.
///
/// The engine covers [`BrowserLayout::viewport`] and **not** the whole node: the chrome bar
/// above it stays Velm's, drawn by wgpu, so the address, the reload button and the ↗ badge
/// are still clickable on a node whose page swallows every event inside itself.
fn viewport_pixels(camera: &Camera, placement: &Placement, laid: &BrowserLayout) -> PixelRect {
    let (width, height) = placement.scaled_size();
    // `x`/`y` are the item's centre; a node's own rectangles are measured from its
    // top-left and are already scaled — the same arrangement `draw.rs` relies on.
    let left = placement.x - width / 2.0 + laid.viewport.x;
    let top = placement.y - height / 2.0 + laid.viewport.y;
    let min = camera.world_to_screen(WorldPoint::new(left, top));
    let max = camera
        .world_to_screen(WorldPoint::new(left + laid.viewport.width, top + laid.viewport.height));
    PixelRect::new(min.x, min.y, max.x - min.x, max.y - min.y)
}

// ---------------------------------------------------------------------------------------
// The engine seam
// ---------------------------------------------------------------------------------------

/// What went wrong, in words a card can show.
///
/// A `String` rather than an enum: every one of these came from a platform and is only
/// ever displayed. What matters is that there is no variant meaning "nothing happened".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineError(pub String);

impl EngineError {
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }

    pub fn message(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for EngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for EngineError {}

/// One live page.
///
/// **Destruction is [`Drop`]**, deliberately, rather than a `destroy` method. A webview is
/// a native view owned by a window and forgetting to release one leaks a whole engine
/// process; making the pool's `HashMap::remove` sufficient is what stops that being a rule
/// somebody has to remember at four call sites.
pub trait Engine {
    /// Point the page somewhere else. Called only when the address actually changed.
    fn navigate(&mut self, url: &str) -> Result<(), EngineError>;

    /// Fetch the current address again, keeping the node and its engine.
    fn reload(&mut self) -> Result<(), EngineError>;

    /// Move and resize the view, and set the page zoom that keeps one world unit one CSS
    /// pixel. One call for both because they are one statement about scale — setting the
    /// bounds without the zoom is the reflow this module exists to avoid.
    fn set_bounds(&mut self, frame: PixelRect, page_zoom: f64) -> Result<(), EngineError>;

    /// Take the view off the screen, or put it back. Cheap, and keeps the page.
    fn set_visible(&mut self, visible: bool) -> Result<(), EngineError>;
}

/// What a page has told us about itself since the last frame.
///
/// A `Rc<RefCell<…>>` rather than a channel because every one of these runs on the main
/// thread — the platform calls the title handler there and the pool is drained there — so a
/// channel would buy nothing but a `Send` bound the webview cannot satisfy anyway.
#[derive(Debug, Clone, Default)]
pub struct TitleSink(Rc<RefCell<Vec<(ItemId, String)>>>);

impl TitleSink {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&self, id: ItemId, title: String) {
        self.0.borrow_mut().push((id, title));
    }

    fn take(&self) -> Vec<(ItemId, String)> {
        std::mem::take(&mut *self.0.borrow_mut())
    }
}

/// Everything an engine needs to exist.
#[derive(Debug, Clone)]
pub struct Spawn {
    pub id: ItemId,
    pub url: String,
    pub frame: PixelRect,
    pub page_zoom: f64,
}

/// Whatever can make an [`Engine`]: a real webview, nothing at all, or a fake in a test.
///
/// The indirection is what lets the pool's rules — hide versus destroy, when a navigate is
/// worth issuing, what happens to a node that disappears from the board — be tested on a
/// machine with no window, which is where those rules are actually likely to be wrong.
pub trait EngineHost {
    fn create(&self, spawn: &Spawn, titles: &TitleSink) -> Result<Box<dyn Engine>, EngineError>;

    /// Why this host cannot make an engine, when it cannot. `None` means it can.
    fn unavailable(&self) -> Option<&'static str> {
        None
    }
}

/// The host in a build with no engine compiled into it.
///
/// It fails with [`NOT_BUILT`] rather than being absent, so the pool, the reconcile and
/// every rule in this file are exercised identically in both builds and the feature-off
/// path is the one that ships.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoEngine;

impl EngineHost for NoEngine {
    fn create(&self, _spawn: &Spawn, _titles: &TitleSink) -> Result<Box<dyn Engine>, EngineError> {
        Err(EngineError::new(NOT_BUILT))
    }

    fn unavailable(&self) -> Option<&'static str> {
        Some(NOT_BUILT)
    }
}

// ---------------------------------------------------------------------------------------
// The pool
// ---------------------------------------------------------------------------------------

/// What the app wants for one browser node this frame.
#[derive(Debug, Clone)]
pub struct Request {
    pub id: ItemId,
    pub url: String,
    /// `Some` when `crate::browser::should_run_engine` said yes — both switches. `None`
    /// means the user has not asked for this page, and an engine it already had is
    /// **destroyed**: `live` going false is the user saying stop.
    pub placed: Option<Placed>,
}

/// One node's engine and what it was last told.
struct Slot {
    engine: Box<dyn Engine>,
    url: String,
    frame: PixelRect,
    page_zoom: f64,
    visible: bool,
}

/// Why one node is not showing a page. Absent from the map means it is.
#[derive(Debug, Clone)]
enum Reported {
    Hidden(Hidden),
    /// The engine would not start, or the page would not load, and what it said.
    Failed(String),
}

/// Every live page in the application.
///
/// **Empty costs nothing.** No thread, no timer, no allocation beyond an empty map until a
/// browser node asks to run — `docs/07-agent-canvas.md` §0 rule 2, applied to the most
/// expensive thing in the layer.
pub struct BrowserEngines {
    host: Box<dyn EngineHost>,
    live: HashMap<ItemId, Slot>,
    /// What each node is doing that is not showing a page.
    ///
    /// **One map, not one per reason.** The painter asks this the same frame the reconcile
    /// wrote it, so a second map recording "failed" beside one recording "hidden" is two
    /// sources for one sentence — the failure this repository has already paid for twice.
    /// A failure is also what stops the engine being asked again every frame; clearing the
    /// entry is therefore the only retry, and it is deliberately reachable (see
    /// [`BrowserEngines::reload`]) rather than permanent — `Assets::Undecodable` never
    /// retried and an icon never arrived for the life of a session.
    reported: HashMap<ItemId, Reported>,
    titles: TitleSink,
}

impl BrowserEngines {
    pub fn new(host: Box<dyn EngineHost>) -> Self {
        Self {
            host,
            live: HashMap::new(),
            reported: HashMap::new(),
            titles: TitleSink::new(),
        }
    }

    /// A pool that can never make an engine. What a build without the feature gets.
    pub fn unavailable() -> Self {
        Self::new(Box::new(NoEngine))
    }

    /// How many pages are loaded, visible or not. For the HUD and for tests.
    pub fn live(&self) -> usize {
        self.live.len()
    }

    /// Whether anything at all is running. `docs/07` §0 rule 2's own question.
    pub fn dormant(&self) -> bool {
        self.live.is_empty()
    }

    /// Why no engine can be made here, if none can.
    pub fn unavailable_reason(&self) -> Option<&'static str> {
        self.host.unavailable()
    }

    /// What this node should say instead of a page, if anything.
    ///
    /// Asked by the painter, so the card and the engine cannot come to disagree about
    /// whether a page is up — which would show as an engine running behind a node that
    /// says it is not, or a sentence printed under a page that is fine. `None` means either
    /// that a page is showing or that the reason is not worth saying: see
    /// [`Hidden::worth_saying`].
    pub fn message(&self, id: ItemId) -> Option<String> {
        match self.reported.get(&id)? {
            Reported::Failed(error) => Some(error.clone()),
            Reported::Hidden(reason) => reason.worth_saying().then(|| reason.reason().to_owned()),
        }
    }

    /// Brings every engine into line with what the board says, once per frame.
    ///
    /// **Driven by the document, not by events.** A node that is deleted, or undone back
    /// out of existence, simply stops appearing in `requests` and its engine goes with it —
    /// which is one rule instead of a hook on delete, a hook on undo, a hook on cut and a
    /// fourth one somebody forgets.
    pub fn reconcile(&mut self, requests: &[Request]) {
        // Anything the board no longer asks for. Dropping the `Slot` drops the `Engine`,
        // which is what releases the native view — see the trait's own note.
        let wanted: std::collections::HashSet<ItemId> =
            requests.iter().map(|request| request.id).collect();
        self.live.retain(|id, _| wanted.contains(id));
        self.reported.retain(|id, _| wanted.contains(id));

        for request in requests {
            match &request.placed {
                // The user has not asked for this page, or has just un-asked. Stop.
                None => {
                    self.live.remove(&request.id);
                    self.reported.remove(&request.id);
                }
                Some(Placed::Hide(reason)) => {
                    // Hidden, not destroyed: the scroll position and any half-filled form
                    // survive being panned past. An engine that does not exist yet is not
                    // created to be immediately hidden — a page nobody can see is not worth
                    // 60MB and a network request.
                    if let Some(slot) = self.live.get_mut(&request.id)
                        && slot.visible
                    {
                        slot.visible = false;
                        if let Err(error) = slot.engine.set_visible(false) {
                            log::warn!("a browser node would not hide: {error}");
                        }
                    }
                    // A failure outranks a hide: an engine that refused to start has
                    // something to say that panning past it does not erase.
                    if !matches!(self.reported.get(&request.id), Some(Reported::Failed(_))) {
                        self.reported.insert(request.id, Reported::Hidden(*reason));
                    }
                }
                Some(Placed::Show { frame, page_zoom }) => {
                    self.show(request, *frame, *page_zoom);
                }
            }
        }
    }

    fn show(&mut self, request: &Request, frame: PixelRect, page_zoom: f64) {
        if matches!(self.reported.get(&request.id), Some(Reported::Failed(_))) {
            // Reported once and left alone. Retrying a refusing engine every frame is a
            // request storm at 100fps against somebody else's server.
            return;
        }
        if !self.live.contains_key(&request.id) {
            let spawn = Spawn {
                id: request.id,
                url: request.url.clone(),
                frame,
                page_zoom,
            };
            match self.host.create(&spawn, &self.titles) {
                Ok(engine) => {
                    self.live.insert(
                        request.id,
                        Slot {
                            engine,
                            url: request.url.clone(),
                            frame,
                            page_zoom,
                            visible: true,
                        },
                    );
                    self.reported.remove(&request.id);
                }
                Err(error) => {
                    log::warn!("a browser node would not start: {error}");
                    self.reported.insert(request.id, Reported::Failed(error.0));
                }
            }
            return;
        }

        let Some(slot) = self.live.get_mut(&request.id) else { return };
        if slot.url != request.url {
            // Only when it actually changed. Re-issuing the same address every frame would
            // reload the page continuously, which looks like a page that never finishes.
            if let Err(error) = slot.engine.navigate(&request.url) {
                log::warn!("a browser node would not navigate: {error}");
            }
            slot.url.clone_from(&request.url);
        }
        if slot.frame != frame || slot.page_zoom != page_zoom {
            if let Err(error) = slot.engine.set_bounds(frame, page_zoom) {
                log::warn!("a browser node would not move: {error}");
            }
            slot.frame = frame;
            slot.page_zoom = page_zoom;
        }
        if !slot.visible {
            slot.visible = true;
            if let Err(error) = slot.engine.set_visible(true) {
                log::warn!("a browser node would not show: {error}");
            }
        }
        self.reported.remove(&request.id);
    }

    /// Fetches one node's page again. The engine and its history stay.
    pub fn reload(&mut self, id: ItemId) -> Result<(), EngineError> {
        // A node whose engine failed to start is worth retrying here: the user pressed a
        // button, which is a different thing from a frame going past.
        self.reported.remove(&id);
        match self.live.get_mut(&id) {
            Some(slot) => slot.engine.reload(),
            None => Err(EngineError::new("there is no page loaded on this node")),
        }
    }

    /// Stops every engine.
    ///
    /// The three ways a page must stop without the node changing: the board's tab is
    /// switched away (a parked board must not keep a page composited over the board in
    /// front of it — the view belongs to the *window*, not to the board), the preference is
    /// turned off, and the application quits.
    pub fn destroy_all(&mut self) {
        self.live.clear();
        self.reported.clear();
    }

    /// Titles the pages have reported since the last frame.
    ///
    /// **Drained, never applied here.** Writing a title is a document edit, and a document
    /// edit made from a background callback is the undo-group leak `apply_link_fetches`
    /// already paid for (feedback 30): a page finishing its load while a caret is up would
    /// re-raise *"There is already an active undo group"* and break every operation after
    /// it. The caller applies these where it can wait for the caret.
    pub fn take_title_updates(&mut self) -> Vec<(ItemId, String)> {
        self.titles.take()
    }
}

impl Default for BrowserEngines {
    fn default() -> Self {
        Self::unavailable()
    }
}

impl std::fmt::Debug for BrowserEngines {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BrowserEngines")
            .field("live", &self.live.len())
            .field("reported", &self.reported.len())
            .finish()
    }
}

// ---------------------------------------------------------------------------------------
// The real engine
// ---------------------------------------------------------------------------------------

/// The only code in Velm that instantiates a web engine, and it is behind a cargo feature
/// that is off by default.
///
/// Nothing above this module changes shape when the feature moves: the pool holds a
/// `Box<dyn EngineHost>` either way, and the feature-off build ships [`NoEngine`], so the
/// path that runs for every user is the path the tests exercise.
#[cfg(feature = "browser")]
pub mod wry_host {
    use std::sync::Arc;

    use winit::window::Window;
    use wry::dpi::{PhysicalPosition, PhysicalSize};
    use wry::{Rect as WryRect, WebView, WebViewBuilder};

    use super::{Engine, EngineError, EngineHost, PixelRect, Spawn, TitleSink};

    /// Makes a `WKWebView` (macOS) or a WebView2 controller (Windows) as a child of Velm's
    /// own window.
    ///
    /// # Why a child of the window rather than a window of its own
    ///
    /// A separate window would not move with the board, would not be clipped by anything,
    /// and would appear in the window list and in ⌘`. A child view composites above the
    /// `CAMetalLayer` wgpu draws into — which is the whole reason this cannot be occluded
    /// by a frame, and is also why it works at all.
    pub struct WryHost {
        window: Arc<Window>,
    }

    impl WryHost {
        pub fn new(window: Arc<Window>) -> Self {
            Self { window }
        }
    }

    /// A live page, and the platform view under it.
    ///
    /// Dropping this drops the `WebView`, which removes the native view from the window.
    /// That is the whole destruction path — see [`Engine`]'s note on why it is `Drop`.
    pub struct WryEngine {
        webview: WebView,
    }

    fn bounds(frame: PixelRect) -> WryRect {
        // Physical, straight through. `wry` takes `dpi::Physical*` and converts using the
        // window's own scale factor; converting to points here as well would apply the
        // factor twice, which is `CLAUDE.md` trap 4 with a native view on the end of it.
        WryRect {
            position: PhysicalPosition::new(frame.x, frame.y).into(),
            size: PhysicalSize::new(frame.width.max(1.0), frame.height.max(1.0)).into(),
        }
    }

    impl EngineHost for WryHost {
        fn create(
            &self,
            spawn: &Spawn,
            titles: &TitleSink,
        ) -> Result<Box<dyn Engine>, EngineError> {
            let sink = titles.clone();
            let id = spawn.id;
            let mut builder = WebViewBuilder::new()
                .with_bounds(bounds(spawn.frame))
                // The page reports its own name, so a node reads as the site rather than as
                // the URL. Applied by the caller, never written from in here: see
                // `BrowserEngines::take_title_updates`.
                .with_document_title_changed_handler(move |title| {
                    sink.push(id, title);
                });
            if !spawn.url.trim().is_empty() {
                builder = builder.with_url(spawn.url.clone());
            }
            // `as_ref()`, not `&self.window`: the parameter is `&W where W: HasWindowHandle`,
            // so passing the `Arc` itself makes `W = Arc<Window>` and leans on an impl that
            // may not exist. `&Window` is the one that certainly does.
            let webview = builder
                .build_as_child(self.window.as_ref())
                .map_err(|error| EngineError::new(format!("{error}")))?;

            let mut engine = WryEngine { webview };
            // The page zoom is the whole scale story (see the module header) and the
            // builder has no `with_zoom`, so it is set immediately after the view exists.
            engine.set_bounds(spawn.frame, spawn.page_zoom)?;
            Ok(Box::new(engine))
        }
    }

    impl Engine for WryEngine {
        fn navigate(&mut self, url: &str) -> Result<(), EngineError> {
            self.webview.load_url(url).map_err(|error| EngineError::new(format!("{error}")))
        }

        fn reload(&mut self) -> Result<(), EngineError> {
            self.webview.reload().map_err(|error| EngineError::new(format!("{error}")))
        }

        fn set_bounds(&mut self, frame: PixelRect, page_zoom: f64) -> Result<(), EngineError> {
            self.webview
                .set_bounds(bounds(frame))
                .map_err(|error| EngineError::new(format!("{error}")))?;
            self.webview.zoom(page_zoom).map_err(|error| EngineError::new(format!("{error}")))
        }

        fn set_visible(&mut self, visible: bool) -> Result<(), EngineError> {
            self.webview
                .set_visible(visible)
                .map_err(|error| EngineError::new(format!("{error}")))
        }
    }
}

#[cfg(test)]
mod tests {
    use vellum_scene::{ScreenPoint, ScreenSize};

    use super::*;

    const WINDOW: (f64, f64) = (1600.0, 1000.0);

    fn camera(zoom: f64) -> Camera {
        let mut camera = Camera::new(ScreenSize::new(WINDOW.0, WINDOW.1));
        camera.set_zoom_about(zoom, ScreenPoint::new(WINDOW.0 / 2.0, WINDOW.1 / 2.0));
        camera
    }

    fn node() -> Placement {
        Placement::new(0.0, 0.0, crate::browser::DEFAULT_SIZE.0, crate::browser::DEFAULT_SIZE.1)
    }

    /// A node small enough that it still fits the window when the board is zoomed right
    /// in. The default 720 × 480 does not — 720 units at 8× is 5,760 pixels — and a
    /// containment failure would be read as a page-zoom failure.
    fn small_node() -> Placement {
        Placement::new(0.0, 0.0, 200.0, 120.0)
    }

    fn frame(keep_out: &[PixelRect]) -> ViewFrame<'_> {
        ViewFrame {
            canvas: PixelRect::new(0.0, 0.0, WINDOW.0, WINDOW.1),
            // The display this was written on: physical pixels are twice the points, which
            // is the factor a logical/physical mix-up is out by (trap 4).
            scale: 2.0,
            keep_out,
            suspended: false,
        }
    }

    // ----- the arithmetic ---------------------------------------------------------------

    /// The claim the whole scale story rests on: the page's CSS viewport is the node's own
    /// size in world units, **at every zoom**. If this drifts, a responsive page reflows
    /// mid-pinch and the node shows different content at different zooms.
    #[test]
    fn one_world_unit_is_one_css_pixel_at_every_zoom() {
        let placement = node();
        let laid = layout(placement.width, placement.height);
        // Up to the zoom at which the default node stops fitting the test window; the
        // clamp above that has its own test, on a node that does fit.
        for zoom in [0.6, 1.0, 2.0] {
            let view = frame(&[]);
            let Placed::Show { frame: rect, page_zoom } = place(&camera(zoom), &placement, &view)
            else {
                panic!("no page at zoom {zoom}");
            };
            // The frame in points, divided by the page zoom, is the page's CSS width.
            let css = rect.width / view.scale / page_zoom;
            assert!(
                (css - laid.viewport.width).abs() < 0.001,
                "at zoom {zoom} the page saw {css} CSS px for a {} unit node",
                laid.viewport.width
            );
        }
    }

    /// The engine covers the page area only. The chrome bar has to stay Velm's, because a
    /// native view swallows every event over itself and that bar is then the only part of a
    /// live node that can still be clicked.
    #[test]
    fn the_engine_never_covers_the_nodes_own_chrome_bar() {
        let placement = node();
        let laid = layout(placement.width, placement.height);
        let Placed::Show { frame: rect, .. } = place(&camera(2.0), &placement, &frame(&[])) else {
            panic!("no page");
        };
        let bar_bottom_world = placement.y - placement.height / 2.0 + laid.bar.y + laid.bar.height;
        let bar_bottom = camera(2.0).world_to_screen(WorldPoint::new(0.0, bar_bottom_world)).y;
        assert!(
            rect.y >= bar_bottom,
            "the page started at {} and the chrome bar ran to {bar_bottom}",
            rect.y
        );
    }

    /// A clipped frame shows the page's *top-left corner* in a smaller box, not the
    /// clipped portion of the page — the one failure mode that is actively misleading. So
    /// a node that is not wholly on the canvas hides.
    #[test]
    fn a_node_hanging_over_the_edge_hides_rather_than_being_clipped() {
        let mut placement = node();
        let view = frame(&[]);
        assert!(matches!(place(&camera(1.0), &placement, &view), Placed::Show { .. }));

        // Far enough left that its left edge is off the canvas, near enough that most of it
        // is still on: the case a clip would get wrong.
        placement.x = -(WINDOW.0 / 2.0) + 100.0;
        assert_eq!(
            place(&camera(1.0), &placement, &view),
            Placed::Hide(Hidden::PartlyOffCanvas)
        );

        placement.x = -10_000.0;
        assert_eq!(place(&camera(1.0), &placement, &view), Placed::Hide(Hidden::OffScreen));
    }

    /// The floor is legibility, and it must bite before the page becomes mush rather than
    /// after. A fitted board — 4% — must not be running an engine.
    #[test]
    fn a_zoomed_out_board_shows_the_card_instead_of_an_unreadable_page() {
        let placement = node();
        let view = frame(&[]);
        assert_eq!(place(&camera(0.04), &placement, &view), Placed::Hide(Hidden::ZoomedOut));
        assert_eq!(place(&camera(0.4), &placement, &view), Placed::Hide(Hidden::ZoomedOut));
        assert!(matches!(place(&camera(0.6), &placement, &view), Placed::Show { .. }));
    }

    /// Clamped rather than refused: past the ceiling the page reflows wider, exactly as a
    /// browser at 300% does, which is a legible page rather than a wrong one.
    #[test]
    fn zooming_far_in_clamps_the_page_zoom_and_keeps_drawing() {
        let placement = small_node();
        let Placed::Show { page_zoom, .. } = place(&camera(8.0), &placement, &frame(&[])) else {
            panic!("a very zoomed-in board stopped showing its page");
        };
        assert!(
            page_zoom_for(8.0, 2.0) > MAX_PAGE_ZOOM,
            "the fixture no longer asks for more zoom than the ceiling allows"
        );
        assert_eq!(page_zoom, MAX_PAGE_ZOOM);
    }

    /// A native view is composited above everything Velm paints, so anything of ours that
    /// floats over the canvas has to take the page off the screen — not the other way
    /// round, which is a toolbar with a web page drawn over it.
    #[test]
    fn velms_own_floating_chrome_takes_the_page_off_the_screen() {
        let placement = node();
        // The node's page runs from x=448 to x=1152 at this zoom, so a palette has to be
        // 500 wide to reach it. Both figures are chosen against that, not guessed.
        let palette = [PixelRect::new(0.0, 0.0, 500.0, WINDOW.1)];
        assert_eq!(
            place(&camera(1.0), &placement, &frame(&palette)),
            Placed::Hide(Hidden::Covered)
        );

        // A palette that does not reach the node leaves it alone.
        let far = [PixelRect::new(0.0, 0.0, 400.0, WINDOW.1)];
        assert!(matches!(place(&camera(1.0), &placement, &frame(&far)), Placed::Show { .. }));

        let mut suspended = frame(&[]);
        suspended.suspended = true;
        assert_eq!(place(&camera(1.0), &placement, &suspended), Placed::Hide(Hidden::Suspended));
    }

    /// A native view has no rotation to give, and the axis-aligned arithmetic above is only
    /// exact for an unrotated node — so this is a correctness gate, not a preference.
    #[test]
    fn a_rotated_node_shows_its_card() {
        let mut placement = node();
        placement.rotation = 12.0;
        assert_eq!(place(&camera(1.0), &placement, &frame(&[])), Placed::Hide(Hidden::Rotated));
    }

    #[test]
    fn a_node_too_small_for_a_page_says_so_rather_than_drawing_a_sliver() {
        let mut placement = node();
        placement.width = crate::browser::MIN_SIZE.0 - 10.0;
        placement.height = crate::browser::MIN_SIZE.1 - 10.0;
        assert_eq!(place(&camera(1.0), &placement, &frame(&[])), Placed::Hide(Hidden::TooSmall));

        // Small in world units but the board zoomed right in: legible, so it draws. The
        // pixel floor and the world floor are different questions and both are asked.
        let mut small = node();
        small.width = crate::browser::MIN_SIZE.0 + 4.0;
        small.height = crate::browser::MIN_SIZE.1 + 4.0;
        assert!(matches!(place(&camera(2.0), &small, &frame(&[])), Placed::Show { .. }));
    }

    /// Nothing in this app is inert. Every way a page can be absent has words, and two
    /// remedies never share a sentence — a user who reads *"zoom in"* when the answer is
    /// *"the node is rotated"* is worse off than one who reads nothing.
    #[test]
    fn every_reason_a_page_is_missing_has_its_own_words() {
        let all = [
            Hidden::OffScreen,
            Hidden::PartlyOffCanvas,
            Hidden::ZoomedOut,
            Hidden::TooSmall,
            Hidden::Rotated,
            Hidden::Covered,
            Hidden::Suspended,
        ];
        let mut seen = std::collections::HashSet::new();
        for reason in all {
            assert!(!reason.reason().is_empty(), "{reason:?} had nothing to say");
            assert!(seen.insert(reason.reason()), "{reason:?} repeated another reason's words");
        }
        assert!(!Hidden::OffScreen.worth_saying(), "a node nobody can see explained itself");
        assert!(Hidden::Rotated.worth_saying());
    }

    // ----- the pool ---------------------------------------------------------------------

    /// What a fake engine was told, in order. The pool's rules are about *which* calls are
    /// made and when, so a recording is the only thing that can assert them.
    #[derive(Debug, Default)]
    struct Log {
        calls: Vec<String>,
        created: usize,
        dropped: usize,
        fail_next: Option<String>,
    }

    #[derive(Clone, Default)]
    struct FakeHost(Rc<RefCell<Log>>);

    struct FakeEngine {
        id: ItemId,
        log: Rc<RefCell<Log>>,
    }

    impl EngineHost for FakeHost {
        fn create(
            &self,
            spawn: &Spawn,
            titles: &TitleSink,
        ) -> Result<Box<dyn Engine>, EngineError> {
            if let Some(message) = self.0.borrow_mut().fail_next.take() {
                return Err(EngineError::new(message));
            }
            {
                let mut log = self.0.borrow_mut();
                log.created += 1;
                log.calls.push(format!("create {}", spawn.url));
            }
            // A real engine reports the page's name once it has one; the fake reports it
            // immediately, which is what makes the drain testable.
            titles.push(spawn.id, format!("{} — title", spawn.url));
            Ok(Box::new(FakeEngine { id: spawn.id, log: Rc::clone(&self.0) }))
        }
    }

    impl Engine for FakeEngine {
        fn navigate(&mut self, url: &str) -> Result<(), EngineError> {
            self.log.borrow_mut().calls.push(format!("navigate {url}"));
            Ok(())
        }

        fn reload(&mut self) -> Result<(), EngineError> {
            self.log.borrow_mut().calls.push("reload".into());
            Ok(())
        }

        fn set_bounds(&mut self, frame: PixelRect, page_zoom: f64) -> Result<(), EngineError> {
            self.log
                .borrow_mut()
                .calls
                .push(format!("bounds {}x{} @{page_zoom}", frame.width, frame.height));
            Ok(())
        }

        fn set_visible(&mut self, visible: bool) -> Result<(), EngineError> {
            self.log.borrow_mut().calls.push(format!("visible {visible}"));
            Ok(())
        }
    }

    impl Drop for FakeEngine {
        fn drop(&mut self) {
            let mut log = self.log.borrow_mut();
            log.dropped += 1;
            log.calls.push(format!("drop {:?}", self.id));
        }
    }

    fn ids(count: usize) -> Vec<ItemId> {
        // `ItemId` has no public constructor, so the ids come from a real document — which
        // is cheap, in memory, and is also what the app will hand this.
        let mut board = vellum_doc::Board::new();
        (0..count)
            .map(|_| {
                board
                    .add(vellum_doc::NewItem::new(
                        vellum_doc::ItemKind::Browser { model: String::new() },
                        Placement::new(0.0, 0.0, 10.0, 10.0),
                    ))
                    .expect("an item")
            })
            .collect()
    }

    fn shown(id: ItemId, url: &str) -> Request {
        Request {
            id,
            url: url.into(),
            placed: Some(Placed::Show {
                frame: PixelRect::new(0.0, 0.0, 720.0, 400.0),
                page_zoom: 1.0,
            }),
        }
    }

    /// The rule that costs the user something if it is got wrong: a page panned off screen
    /// is **hidden**, so its scroll position and any half-filled form survive coming back;
    /// a page the user turned off is **destroyed**.
    #[test]
    fn scrolling_past_a_page_hides_it_and_turning_it_off_destroys_it() {
        let host = FakeHost::default();
        let log = Rc::clone(&host.0);
        let mut pool = BrowserEngines::new(Box::new(host));
        let id = ids(1)[0];

        pool.reconcile(&[shown(id, "https://example.test/")]);
        assert_eq!(pool.live(), 1);
        assert_eq!(log.borrow().created, 1);

        // Off screen: hidden, and the engine survives.
        pool.reconcile(&[Request {
            placed: Some(Placed::Hide(Hidden::OffScreen)),
            ..shown(id, "https://example.test/")
        }]);
        assert_eq!(pool.live(), 1, "panning past a page destroyed it and lost its state");
        assert_eq!(log.borrow().dropped, 0);
        assert!(log.borrow().calls.contains(&"visible false".to_string()));

        // Back on screen: shown again, and **not** created again.
        pool.reconcile(&[shown(id, "https://example.test/")]);
        assert_eq!(log.borrow().created, 1, "coming back on screen reloaded the page");
        assert!(log.borrow().calls.contains(&"visible true".to_string()));

        // The user cleared `live`. That is a stop.
        pool.reconcile(&[Request { placed: None, ..shown(id, "https://example.test/") }]);
        assert_eq!(pool.live(), 0);
        assert_eq!(log.borrow().dropped, 1, "turning a page off left its engine running");
    }

    /// Lifecycle comes from the document rather than from a hook on every verb — so delete,
    /// cut and undo-of-create all release their engine without any of them knowing this
    /// module exists.
    #[test]
    fn a_node_that_leaves_the_board_takes_its_engine_with_it() {
        let host = FakeHost::default();
        let log = Rc::clone(&host.0);
        let mut pool = BrowserEngines::new(Box::new(host));
        let all = ids(2);

        pool.reconcile(&[shown(all[0], "https://a.test/"), shown(all[1], "https://b.test/")]);
        assert_eq!(pool.live(), 2);

        pool.reconcile(&[shown(all[0], "https://a.test/")]);
        assert_eq!(pool.live(), 1, "a deleted node kept its engine");
        assert_eq!(log.borrow().dropped, 1);

        // A tab switch: the view belongs to the window, so a parked board must not keep a
        // page composited over the board in front of it.
        pool.destroy_all();
        assert_eq!(pool.live(), 0);
        assert_eq!(log.borrow().dropped, 2);
        assert!(pool.dormant(), "an empty pool should cost nothing");
    }

    /// Re-issuing the same address every frame reloads the page continuously, which reads
    /// as a page that never finishes loading. And moving costs a call only when it moved.
    #[test]
    fn a_frame_where_nothing_changed_says_nothing_to_the_engine() {
        let host = FakeHost::default();
        let log = Rc::clone(&host.0);
        let mut pool = BrowserEngines::new(Box::new(host));
        let id = ids(1)[0];

        pool.reconcile(&[shown(id, "https://example.test/")]);
        let after_create = log.borrow().calls.len();
        for _ in 0..10 {
            pool.reconcile(&[shown(id, "https://example.test/")]);
        }
        assert_eq!(
            log.borrow().calls.len(),
            after_create,
            "an idle frame talked to the engine: {:?}",
            log.borrow().calls
        );

        // A real address change does reach it, exactly once.
        pool.reconcile(&[shown(id, "https://other.test/")]);
        pool.reconcile(&[shown(id, "https://other.test/")]);
        let navigations =
            log.borrow().calls.iter().filter(|call| call.starts_with("navigate")).count();
        assert_eq!(navigations, 1, "{:?}", log.borrow().calls);
    }

    /// An engine that refuses degrades to the card with what it said, and is not asked
    /// again every frame — which at 100fps is a request storm against somebody else's
    /// server. Pressing reload *is* a retry, because the user asked.
    #[test]
    fn an_engine_that_will_not_start_reports_it_once_and_stops_asking() {
        let host = FakeHost::default();
        let log = Rc::clone(&host.0);
        log.borrow_mut().fail_next = Some("no engine here".into());
        let mut pool = BrowserEngines::new(Box::new(host));
        let id = ids(1)[0];

        for _ in 0..5 {
            pool.reconcile(&[shown(id, "https://example.test/")]);
        }
        assert_eq!(pool.live(), 0);
        assert_eq!(log.borrow().created, 0);
        assert_eq!(pool.message(id).as_deref(), Some("no engine here"));

        // The user pressed reload: try again.
        assert!(pool.reload(id).is_err(), "there is nothing loaded to reload yet");
        pool.reconcile(&[shown(id, "https://example.test/")]);
        assert_eq!(pool.live(), 1, "reload did not clear the failure");
        assert_eq!(pool.message(id), None);
    }

    /// A build with no engine compiled in is not a build where the node is inert: it is a
    /// build where the node says which of the three things is missing.
    #[test]
    fn a_build_with_no_engine_says_so_and_stays_empty() {
        let mut pool = BrowserEngines::unavailable();
        let id = ids(1)[0];
        assert_eq!(pool.unavailable_reason(), Some(NOT_BUILT));

        pool.reconcile(&[shown(id, "https://example.test/")]);
        assert_eq!(pool.live(), 0);
        assert_eq!(pool.message(id).as_deref(), Some(NOT_BUILT));
        assert!(NOT_BUILT.contains("your own browser"), "the escape hatch was not named");
    }

    /// The painter and the pool are one derivation of "is there a page on this node".
    ///
    /// A node showing a page says nothing; a node hidden for a reason the user can act on
    /// says it; a node hidden for a reason they cannot — panned off screen, or under a menu
    /// that is about to close — stays quiet rather than flickering a sentence.
    #[test]
    fn the_card_says_exactly_what_the_pool_is_doing() {
        let host = FakeHost::default();
        let mut pool = BrowserEngines::new(Box::new(host));
        let id = ids(1)[0];
        let url = "https://example.test/";

        pool.reconcile(&[shown(id, url)]);
        assert_eq!(pool.message(id), None, "a page that is up was written over");

        for (reason, expected) in [
            (Hidden::ZoomedOut, Some(Hidden::ZoomedOut.reason().to_owned())),
            (Hidden::Rotated, Some(Hidden::Rotated.reason().to_owned())),
            (Hidden::OffScreen, None),
            (Hidden::Suspended, None),
        ] {
            pool.reconcile(&[Request { placed: Some(Placed::Hide(reason)), ..shown(id, url) }]);
            assert_eq!(pool.message(id), expected, "{reason:?} said the wrong thing");
        }

        // …and coming back puts it quiet again, rather than leaving the last excuse up.
        pool.reconcile(&[shown(id, url)]);
        assert_eq!(pool.message(id), None);
    }

    /// A title is *drained*, never applied from in here. A page finishing its load while a
    /// caret is up must not write to the document — that is the undo-group leak
    /// `apply_link_fetches` already paid for.
    #[test]
    fn titles_are_handed_back_rather_than_written() {
        let host = FakeHost::default();
        let mut pool = BrowserEngines::new(Box::new(host));
        let id = ids(1)[0];

        pool.reconcile(&[shown(id, "https://example.test/")]);
        let titles = pool.take_title_updates();
        assert_eq!(titles.len(), 1);
        assert_eq!(titles[0].0, id);
        assert!(titles[0].1.contains("example.test"));
        assert!(pool.take_title_updates().is_empty(), "a title was handed back twice");
    }
}
